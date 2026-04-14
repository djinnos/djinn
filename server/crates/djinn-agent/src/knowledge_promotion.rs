use std::path::{Path, PathBuf};

use djinn_db::repositories::note::{WorktreeNoteChangeKind, WorktreeNoteDiff};
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
    pub worktree_path: Option<String>,
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

fn change_kind_label(kind: &WorktreeNoteChangeKind) -> &'static str {
    match kind {
        WorktreeNoteChangeKind::Added => "added",
        WorktreeNoteChangeKind::Modified => "modified",
        WorktreeNoteChangeKind::Unchanged => "unchanged",
    }
}

fn preview_candidate(diff: WorktreeNoteDiff) -> KnowledgePromotionNoteCandidate {
    KnowledgePromotionNoteCandidate {
        permalink: diff.permalink,
        title: diff.title,
        note_type: diff.note_type,
        change_kind: change_kind_label(&diff.change_kind).to_string(),
        canonical_note_id: diff.canonical_note_id,
        canonical_file_exists: diff.canonical_file_exists,
    }
}

async fn project_and_worktree_for_task(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<(String, String, String, PathBuf)> {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let session_repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    let task = task_repo.get(task_id).await.ok().flatten()?;
    let project_path = project_repo
        .get_path(&task.project_id)
        .await
        .ok()
        .flatten()?;
    let fallback_project_path = project_path.clone();
    let worktree = session_repo
        .latest_worktree_path_for_task(task_id)
        .await
        .ok()
        .flatten();
    let task_short_id = task.short_id.clone();
    Some((
        task.project_id,
        task_short_id.clone(),
        project_path,
        worktree.map(PathBuf::from).unwrap_or_else(|| {
            Path::new(&fallback_project_path)
                .join(".djinn")
                .join("worktrees")
                .join(&task_short_id)
        }),
    ))
}

pub async fn preview_task_knowledge_promotion(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<KnowledgePromotionPreview> {
    let (project_id, _task_short_id, project_path, worktree_path) =
        project_and_worktree_for_task(task_id, app_state).await?;
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let session_repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    let changed_notes = note_repo
        .diff_worktree_notes_against_canonical(
            &project_id,
            Path::new(&project_path),
            &worktree_path,
        )
        .await
        .ok()?
        .into_iter()
        .filter(|diff| diff.change_kind != WorktreeNoteChangeKind::Unchanged)
        .map(preview_candidate)
        .collect::<Vec<_>>();

    let extraction_quality = session_repo
        .latest_event_taxonomy_for_task(task_id)
        .await
        .ok()
        .flatten()
        .and_then(|taxonomy| taxonomy.get("extraction_quality").cloned());

    Some(KnowledgePromotionPreview {
        task_id: task_id.to_string(),
        project_id,
        worktree_path: Some(worktree_path.to_string_lossy().to_string()),
        changed_notes,
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
    let (project_id, task_short_id, project_path, worktree_path) =
        project_and_worktree_for_task(task_id, app_state).await?;
    let preview = preview_task_knowledge_promotion(task_id, app_state).await?;
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let branch = djinn_db::task_branch_name(&task_short_id);

    let promoted_count = match decision {
        KnowledgePromotionDecision::Promote => note_repo
            .sync_worktree_notes_to_canonical(&project_id, Path::new(&project_path), &worktree_path)
            .await
            .unwrap_or(0),
        KnowledgePromotionDecision::Discard => 0,
    };
    let discarded_count = match decision {
        KnowledgePromotionDecision::Promote => 0,
        KnowledgePromotionDecision::Discard => note_repo
            .delete_worktree_notes_from_canonical(&project_id, &worktree_path)
            .await
            .unwrap_or(0),
    };

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
                "promoted_count": promoted_count,
                "discarded_count": discarded_count,
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
        promoted_count,
        discarded_count,
    })
}
