use djinn_memory::{Note, NoteDedupCandidate};
use djinn_db::{NoteRepository, folder_for_type_with_status, note_hash::note_content_hash};
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

pub(crate) struct LlmMemoryWriteDedupDecider {
    runtime: Box<dyn MemoryWriteProviderRuntime>,
}

impl LlmMemoryWriteDedupDecider {
    pub(crate) fn new(db: djinn_db::Database) -> Self {
        Self {
            runtime: Box::new(LlmMemoryWriteProviderRuntime::new(db)),
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

pub(crate) async fn maybe_apply_write_dedup(
    repo: &NoteRepository,
    decider: &dyn MemoryWriteDedupDecider,
    pending: PendingWriteDedup<'_>,
) -> Option<MemoryNoteResponse> {
    match apply_write_dedup(repo, decider, pending).await {
        Ok(response) => response,
        Err(error) => Some(MemoryNoteResponse::error(error)),
    }
}

async fn apply_write_dedup(
    repo: &NoteRepository,
    decider: &dyn MemoryWriteDedupDecider,
    pending: PendingWriteDedup<'_>,
) -> Result<Option<MemoryNoteResponse>, String> {
    if let Some(note) = find_exact_hash_match(repo, pending).await? {
        return Ok(Some(MemoryNoteResponse::deduplicated_from_note(&note)));
    }

    if !mergeable_note_type(pending.note_type) {
        return Ok(None);
    }

    let candidates = lookup_write_dedup_candidates(repo, pending).await?;
    if candidates.is_empty() {
        return Ok(None);
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

    apply_dedup_decision(repo, pending, decision).await
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

pub(crate) async fn apply_dedup_decision(
    repo: &NoteRepository,
    pending: PendingWriteDedup<'_>,
    decision: MemoryWriteDedupDecision,
) -> Result<Option<MemoryNoteResponse>, String> {
    match decision {
        MemoryWriteDedupDecision::CreateNew => Ok(None),
        MemoryWriteDedupDecision::ReuseExisting { candidate_id } => {
            let note = repo
                .get(&candidate_id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("dedup candidate not found: {candidate_id}"))?;
            Ok(Some(MemoryNoteResponse::deduplicated_from_note(&note)))
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
            Ok(Some(MemoryNoteResponse::deduplicated_from_note(&note)))
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
