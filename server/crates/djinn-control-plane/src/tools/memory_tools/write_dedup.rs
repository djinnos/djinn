use djinn_db::{
    NoteRepository, NoteStatus, folder_for_type_with_status, note_hash::note_content_hash,
};
use djinn_memory::{Note, NoteDedupCandidate};
use djinn_provider::CompletionRequest;

use super::MemoryNoteResponse;
use super::write_dedup_prompt::{
    MEMORY_WRITE_DEDUP_SYSTEM, parse_memory_write_dedup_decision, render_memory_write_dedup_prompt,
};
use super::write_dedup_runtime::{LlmMemoryWriteProviderRuntime, MemoryWriteProviderRuntime};
use super::write_dedup_types::{
    MemoryWriteDedupDecider, MemoryWriteDedupDecision, MemoryWriteDedupDecisionInput,
    PendingWriteDedup,
};

const MEMORY_WRITE_DEDUP_MAX_TOKENS: u32 = 768;
const MEMORY_WRITE_DEDUP_CANDIDATE_LIMIT: usize = 5;
const SUPERSEDES_WEIGHT: f64 = 1.0;

pub(crate) struct LlmMemoryWriteDedupDecider {
    runtime: Box<dyn MemoryWriteProviderRuntime>,
}

impl LlmMemoryWriteDedupDecider {
    pub(crate) fn new(db: djinn_db::Database, user_id: Option<String>) -> Self {
        Self {
            runtime: Box::new(LlmMemoryWriteProviderRuntime::new(db, user_id)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_runtime(runtime: Box<dyn MemoryWriteProviderRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait]
impl MemoryWriteDedupDecider for LlmMemoryWriteDedupDecider {
    async fn decide(
        &self,
        input: MemoryWriteDedupDecisionInput<'_>,
    ) -> Result<MemoryWriteDedupDecision, String> {
        let prompt = render_memory_write_dedup_prompt(&input);
        let response = self
            .runtime
            .complete(CompletionRequest {
                system: MEMORY_WRITE_DEDUP_SYSTEM.to_string(),
                prompt,
                max_tokens: MEMORY_WRITE_DEDUP_MAX_TOKENS,
            })
            .await?;
        parse_memory_write_dedup_decision(&response.text)
    }
}

/// The dedup decision is deliberately separate from creation so both CreateNew
/// and SupersedeExisting flow through `write_services::create_note`.
pub(crate) enum WriteDedupOutcome {
    CreateNew,
    Respond(Box<MemoryNoteResponse>),
    SupersedeExisting {
        candidate_id: String,
        reason: String,
    },
}

pub(crate) async fn maybe_apply_write_dedup(
    repo: &NoteRepository,
    decider: &dyn MemoryWriteDedupDecider,
    pending: PendingWriteDedup<'_>,
) -> WriteDedupOutcome {
    match apply_write_dedup(repo, decider, pending).await {
        Ok(outcome) => outcome,
        Err(error) => WriteDedupOutcome::Respond(Box::new(MemoryNoteResponse::error(error))),
    }
}

async fn apply_write_dedup(
    repo: &NoteRepository,
    decider: &dyn MemoryWriteDedupDecider,
    pending: PendingWriteDedup<'_>,
) -> Result<WriteDedupOutcome, String> {
    if let Some(note) = find_exact_hash_match(repo, pending).await? {
        emit_decision_kind("reuse_existing");
        return Ok(WriteDedupOutcome::Respond(Box::new(
            MemoryNoteResponse::deduplicated_from_note(&note),
        )));
    }

    if !mergeable_note_type(pending.note_type) {
        emit_decision_kind("create_new");
        return Ok(WriteDedupOutcome::CreateNew);
    }

    let candidates = lookup_write_dedup_candidates(repo, pending).await?;
    if candidates.is_empty() {
        emit_decision_kind("create_new");
        return Ok(WriteDedupOutcome::CreateNew);
    }

    let decision = decider
        .decide(MemoryWriteDedupDecisionInput {
            project_path: pending.project_path,
            title: pending.title,
            content: pending.content,
            note_type: pending.note_type,
            candidates: &candidates,
        })
        .await
        .unwrap_or(MemoryWriteDedupDecision::CreateNew);

    emit_decision_kind(decision.kind());
    apply_dedup_decision(repo, pending, decision).await
}

fn emit_decision_kind(decision_kind: &'static str) {
    tracing::info!(decision_kind, "memory_write dedup decision");
}

async fn find_exact_hash_match(
    repo: &NoteRepository,
    pending: PendingWriteDedup<'_>,
) -> Result<Option<Note>, String> {
    let content_hash = note_content_hash(pending.content);
    repo.find_by_content_hash(pending.project_id, &content_hash)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn lookup_write_dedup_candidates(
    repo: &NoteRepository,
    pending: PendingWriteDedup<'_>,
) -> Result<Vec<NoteDedupCandidate>, String> {
    let folder = folder_for_type_with_status(pending.note_type, pending.status);
    let query_text = format!("{}\n\n{}", pending.title, pending.content);
    repo.dedup_candidates(
        pending.project_id,
        folder,
        pending.note_type,
        &query_text,
        MEMORY_WRITE_DEDUP_CANDIDATE_LIMIT,
    )
    .await
    .map_err(|error| error.to_string())
}

/// The sole write-dedup mutation integration point until a note revision API is
/// available. Merge updates immediately; supersede is completed after the
/// centralized normal note creation path has produced the incoming note.
pub(crate) async fn apply_dedup_decision(
    repo: &NoteRepository,
    pending: PendingWriteDedup<'_>,
    decision: MemoryWriteDedupDecision,
) -> Result<WriteDedupOutcome, String> {
    match decision {
        MemoryWriteDedupDecision::CreateNew => Ok(WriteDedupOutcome::CreateNew),
        MemoryWriteDedupDecision::ReuseExisting { candidate_id } => {
            let note = repo
                .get(&candidate_id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("dedup candidate not found: {candidate_id}"))?;
            Ok(WriteDedupOutcome::Respond(Box::new(
                MemoryNoteResponse::deduplicated_from_note(&note),
            )))
        }
        MemoryWriteDedupDecision::MergeIntoExisting {
            candidate_id,
            merged_title,
            merged_content,
        } => {
            let note = repo
                .update(
                    &candidate_id,
                    &merged_title,
                    &merged_content,
                    pending.tags_json,
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(WriteDedupOutcome::Respond(Box::new(
                MemoryNoteResponse::deduplicated_from_note(&note),
            )))
        }
        MemoryWriteDedupDecision::SupersedeExisting {
            candidate_id,
            reason,
        } => Ok(WriteDedupOutcome::SupersedeExisting {
            candidate_id,
            reason,
        }),
    }
}

/// Complete a supersede after the ordinary creation path has made the canonical
/// incoming note. A guarded active→deprecated transition intentionally treats
/// archived, deprecated, and concurrently superseded targets as successful
/// no-ops; association upsert keeps repeated decisions idempotent.
pub(crate) async fn apply_created_note_supersede(
    repo: &NoteRepository,
    new_note_id: &str,
    candidate_id: &str,
    reason: &str,
) -> Result<(), String> {
    let status_changed = repo
        .set_note_status(candidate_id, NoteStatus::Active, NoteStatus::Deprecated)
        .await
        .map_err(|error| error.to_string())?;
    repo.record_supersedes(new_note_id, candidate_id, SUPERSEDES_WEIGHT)
        .await
        .map_err(|error| error.to_string())?;
    tracing::info!(
        decision_kind = "supersede_existing",
        new_note_id,
        candidate_id,
        status_changed,
        reason,
        "memory_write supersede mutation applied"
    );
    Ok(())
}

impl MemoryWriteDedupDecision {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::CreateNew => "create_new",
            Self::ReuseExisting { .. } => "reuse_existing",
            Self::MergeIntoExisting { .. } => "merge_into_existing",
            Self::SupersedeExisting { .. } => "supersede_existing",
        }
    }
}

pub(crate) fn mergeable_note_type(note_type: &str) -> bool {
    matches!(
        note_type,
        "pattern"
            | "case"
            | "pitfall"
            | "adr"
            | "design"
            | "reference"
            | "requirement"
            | "session"
            | "persona"
            | "journey"
            | "design_spec"
            | "competitive"
            | "tech_spike"
    )
}
