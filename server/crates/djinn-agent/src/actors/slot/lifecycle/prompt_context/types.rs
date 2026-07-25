//! Data passed into and returned from final prompt-context assembly.

use std::collections::BTreeMap;
use std::path::Path;

use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_core::models::Task;
use djinn_core::models::task_attempt::TaskAttemptPromptSummary;
use tokio_util::sync::CancellationToken;

use crate::actors::slot::MergeConflictMetadata;
use crate::context::{AgentContext, MemoryIntentPlannerConfig};
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;

/// Narrow repository seam for planner-directed note searches. The production
/// implementation remains [`djinn_db::NoteRepository`], while lifecycle tests
/// can exercise the real prompt assembly path without another orchestrator.
#[async_trait::async_trait]
pub(crate) trait PlannedNoteSearch: Send + Sync {
    async fn search_planned_notes(
        &self,
        project_id: &str,
        task_id: &str,
        query: &str,
        note_type: &str,
    ) -> Result<Vec<djinn_memory::MemorySearchEntityRow>, String>;
}

#[async_trait::async_trait]
impl PlannedNoteSearch for djinn_db::NoteRepository {
    async fn search_planned_notes(
        &self,
        project_id: &str,
        task_id: &str,
        query: &str,
        note_type: &str,
    ) -> Result<Vec<djinn_memory::MemorySearchEntityRow>, String> {
        let note_only = ["note".to_owned()];
        self.search(djinn_db::NoteSearchParams {
            project_id,
            query,
            task_id: Some(task_id),
            folder: None,
            note_type: Some(note_type),
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: Some(&note_only),
        })
        .await
        .map_err(|error| error.to_string())
    }
}

/// Narrow host seam for the attributed planner call. Keeping this separate
/// from the broad supervisor service lets lifecycle tests exercise real prompt
/// assembly without constructing an RPC/provider stack.
#[async_trait::async_trait]
pub(crate) trait MemoryIntentPlannerHost: Send + Sync {
    async fn plan_memory_intents(
        &self,
        request: djinn_supervisor::services::wire::AttributedPlannerRequest,
    ) -> Result<djinn_supervisor::services::wire::PlannerAttemptResult, String>;
}

/// Production adapter from the broad supervisor-services surface to the
/// planner-only lifecycle seam.
pub(crate) struct SupervisorPlannerHost<'a>(pub &'a dyn djinn_supervisor::SupervisorServices);

#[async_trait::async_trait]
impl MemoryIntentPlannerHost for SupervisorPlannerHost<'_> {
    async fn plan_memory_intents(
        &self,
        request: djinn_supervisor::services::wire::AttributedPlannerRequest,
    ) -> Result<djinn_supervisor::services::wire::PlannerAttemptResult, String> {
        djinn_supervisor::SupervisorServices::plan_memory_intents(self.0, request).await
    }
}

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
    /// Exact canonical rows returned by the repository for this session's
    /// combined MCP + skill load attempt, retained for diagnostic rendering.
    pub extension_diagnostics: Vec<ExtensionLoadDiagnosticV1>,
}

/// Real-session-only attribution needed by the gated planner host call.
pub(crate) struct MemoryIntentPlannerInvocation<'a> {
    pub config: &'a MemoryIntentPlannerConfig,
    pub host: &'a dyn MemoryIntentPlannerHost,
    pub session_id: &'a str,
    pub task_run_id: &'a str,
    /// Required host attribution. A missing creator skips the optional planner.
    pub creator_id: Option<&'a str>,
    /// Parsed task acceptance criteria, retained separately from task JSON.
    pub acceptance_criteria: Vec<String>,
    pub resume_compaction_summary: Option<&'a str>,
    /// Optional test seam; production uses the ordinary `NoteRepository`.
    pub planned_note_search: Option<&'a dyn PlannedNoteSearch>,
}

/// Sibling project flagged as relevant (read-only multi-repo, no eager checkout).
#[derive(Debug, Clone)]
pub(crate) struct ReadSourceInfo {
    pub slug: String,
    pub name: String,
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
    pub read_sources: &'a [ReadSourceInfo],
    pub worker_resume_note: Option<&'a str>,
    pub arbiter_directive: Option<&'a str>,
    pub mcp_server_instructions: &'a BTreeMap<String, String>,
    /// Canonical rows returned for the session-associated extension load pass.
    pub extension_diagnostics: &'a [ExtensionLoadDiagnosticV1],
    /// Per-operation signal supplied by the supervisor; deliberately not part
    /// of the broad, reusable `AgentContext`.
    pub cancellation: Option<&'a CancellationToken>,
    pub memory_intent_planner: Option<MemoryIntentPlannerInvocation<'a>>,
    /// `true` when the resolved project `lifecycle.final_verification` plan is
    /// non-empty. Drives the conditional Worker/Reviewer `run_verification`
    /// surface and prompt guidance (epic 1bnj).
    pub final_verification_configured: bool,
}
