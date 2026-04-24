//! Knowledge promotion shim.
//!
//! Pre-cut-over this module diff'd the worktree's `.djinn/*.md` files
//! against the canonical project notes table and let the user promote/
//! discard them on task close. With the db-only knowledge-base cut-over
//! (notes are no longer mirrored to worktrees) memory writes from a task
//! land directly in the canonical `notes` table tagged with the task's
//! embedding branch — there is no "worktree vs canonical" diff to take.
//!
//! The public surface here is preserved so `task_merge.rs` and
//! `extension::handlers::task_admin` keep compiling; the implementations
//! below collapse to no-ops or minimal embedding-branch promote/discard
//! actions. The MCP `propose_*` workflow (separate from KB notes) still
//! handles ADR drafts via on-disk markdown under `decisions/proposed/`.

use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{NoteRepository, ProjectRepository, SessionRepository, TaskRepository};
use serde::{Deserialize, Serialize};

use crate::context::AgentContext;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgePromotionDecision {
    Promote,
    Discard,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeCleanupReason {
    TaskCompleted,
    TaskAbandoned,
    BranchReset,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgePromotionPreview {
    pub task_id: String,
    pub project_id: String,
    pub workspace_path: Option<String>,
    pub changed_notes: Vec<KnowledgePromotionNoteCandidate>,
    pub extraction_quality: Option<serde_json::Value>,
    pub quality_gate_applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgePromotionNoteCandidate {
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub change_kind: String,
    pub canonical_note_id: Option<String>,
    pub canonical_file_exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgePromotionResult {
    pub task_id: String,
    pub decision: KnowledgePromotionDecision,
    pub preview: KnowledgePromotionPreview,
    pub promoted_count: usize,
    pub discarded_count: usize,
}

async fn project_and_workspace_for_task(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<(String, String)> {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let _ = TaskRunRepository::new(app_state.db.clone());

    let task = task_repo.get(task_id).await.ok().flatten()?;
    // Project lookup retained so callers see the same project_id resolution
    // semantics they used to.
    let _ = project_repo.get(&task.project_id).await.ok().flatten()?;
    Some((task.project_id, task.short_id))
}

pub async fn preview_task_knowledge_promotion(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<KnowledgePromotionPreview> {
    let (project_id, _task_short_id) = project_and_workspace_for_task(task_id, app_state).await?;
    let session_repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    let extraction_quality = session_repo
        .latest_event_taxonomy_for_task(task_id)
        .await
        .ok()
        .flatten()
        .and_then(|taxonomy| taxonomy.get("extraction_quality").cloned());

    Some(KnowledgePromotionPreview {
        task_id: task_id.to_string(),
        project_id,
        workspace_path: None,
        // No worktree↔canonical diff exists with db-only KB storage —
        // memory_writes from a task land directly in the canonical table.
        changed_notes: Vec::new(),
        extraction_quality,
        quality_gate_applied: true,
    })
}

pub async fn apply_task_knowledge_decision(
    task_id: &str,
    decision: KnowledgePromotionDecision,
    cleanup_reason: KnowledgeCleanupReason,
    app_state: &AgentContext,
) -> Option<KnowledgePromotionResult> {
    let (_project_id, task_short_id) = project_and_workspace_for_task(task_id, app_state).await?;
    let preview = preview_task_knowledge_promotion(task_id, app_state).await?;
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let branch = djinn_db::task_branch_name(&task_short_id);

    // Without a worktree mirror there is nothing to promote/discard at the
    // notes-table level. The only branch-aware artifact left is the
    // per-task embedding metadata, which we promote into `main` (or delete)
    // depending on the decision so that vector queries reflect the user's
    // choice.
    let embedding_rows = match decision {
        KnowledgePromotionDecision::Promote => note_repo
            .promote_branch_embeddings(&branch, "main")
            .await
            .unwrap_or(0),
        KnowledgePromotionDecision::Discard => note_repo
            .delete_embeddings_for_branch(&branch)
            .await
            .unwrap_or(0),
    };

    let event_type = match decision {
        KnowledgePromotionDecision::Promote => "knowledge_promoted",
        KnowledgePromotionDecision::Discard => "knowledge_discarded",
    };
    let _ = task_repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "system",
            event_type,
            &serde_json::json!({
                "cleanup_reason": cleanup_reason,
                "quality_gate_applied": preview.quality_gate_applied,
                "changed_notes": preview.changed_notes,
                "extraction_quality": preview.extraction_quality,
                "promoted_count": 0,
                "discarded_count": 0,
                "embedding_rows": embedding_rows,
                "embedding_branch": branch,
            })
            .to_string(),
        )
        .await;

    Some(KnowledgePromotionResult {
        task_id: task_id.to_string(),
        decision,
        preview,
        promoted_count: 0,
        discarded_count: 0,
    })
}
