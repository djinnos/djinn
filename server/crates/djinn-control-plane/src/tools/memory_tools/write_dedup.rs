use djinn_db::{
    NoteRepository, NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation,
    NoteRevisionReason, NoteRevisionSubsystem, TrustedNoteRevisionAttribution,
    TrustedNoteRevisionProvenance, folder_for_type_with_status, note_hash::note_content_hash,
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

pub(crate) struct LlmMemoryWriteDedupDecider {
    runtime: Box<dyn MemoryWriteProviderRuntime>,
}

async fn canonical_candidate(
    repo: &NoteRepository,
    project_id: &str,
    candidate_id: &str,
) -> Result<Note, String> {
    let note = repo
        .get(candidate_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("dedup candidate not found: {candidate_id}"))?;
    if note.project_id != project_id {
        return Err(format!("dedup candidate not found: {candidate_id}"));
    }
    Ok(note)
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

/// Apply a dedup decision with dedup-owned attribution. Ordinary incoming
/// creation remains attributed to the MCP caller in `write_services`.
pub(crate) async fn apply_dedup_decision(
    repo: &NoteRepository,
    pending: PendingWriteDedup<'_>,
    decision: MemoryWriteDedupDecision,
) -> Result<WriteDedupOutcome, String> {
    match decision {
        MemoryWriteDedupDecision::CreateNew => Ok(WriteDedupOutcome::CreateNew),
        MemoryWriteDedupDecision::ReuseExisting { candidate_id } => {
            let note = canonical_candidate(repo, pending.project_id, &candidate_id).await?;
            Ok(WriteDedupOutcome::Respond(Box::new(
                MemoryNoteResponse::deduplicated_from_note(&note),
            )))
        }
        MemoryWriteDedupDecision::MergeIntoExisting {
            candidate_id,
            merged_title: _,
            merged_content,
        } => {
            let existing = canonical_candidate(repo, pending.project_id, &candidate_id).await?;
            let result = repo
                .mutate_with_revision(NoteRevisionMutation {
                    project_id: pending.project_id.to_owned(),
                    note_id: Some(candidate_id),
                    event_kind: NoteRevisionEventKind::Updated,
                    desired: NoteRevisionDesiredState::Existing {
                        content: merged_content,
                        confidence: existing.confidence,
                    },
                    attribution: TrustedNoteRevisionAttribution::system(
                        NoteRevisionSubsystem::Dedup,
                    ),
                    provenance: TrustedNoteRevisionProvenance::default(),
                    reason: NoteRevisionReason::new("dedup:merge_into_existing")
                        .expect("dedup reason is non-blank"),
                })
                .await
                .map_err(|error| error.to_string())?;
            let note = result
                .note
                .expect("existing note mutation returns the canonical note");
            Ok(WriteDedupOutcome::Respond(Box::new(
                MemoryNoteResponse::deduplicated_from_note(&note),
            )))
        }
        MemoryWriteDedupDecision::SupersedeExisting {
            candidate_id,
            reason,
        } => {
            let candidate = canonical_candidate(repo, pending.project_id, &candidate_id).await?;
            Ok(WriteDedupOutcome::SupersedeExisting {
                candidate_id: candidate.id,
                reason,
            })
        }
    }
}

/// Complete a supersede after ordinary creation through a Dedup-attributed revision.
pub(crate) async fn apply_created_note_supersede(
    repo: &NoteRepository,
    project_id: &str,
    new_note_id: &str,
    candidate_id: &str,
    reason: &str,
) -> Result<(), String> {
    let candidate = canonical_candidate(repo, project_id, candidate_id).await?;
    let result = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.to_owned(),
            note_id: Some(candidate.id),
            event_kind: NoteRevisionEventKind::Updated,
            desired: NoteRevisionDesiredState::Supersede {
                canonical_note_id: new_note_id.to_owned(),
            },
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Dedup),
            provenance: TrustedNoteRevisionProvenance::default(),
            reason: NoteRevisionReason::new("dedup:supersede_existing")
                .expect("dedup reason is non-blank"),
        })
        .await
        .map_err(|error| error.to_string())?;
    tracing::info!(
        decision_kind = "supersede_existing",
        new_note_id,
        candidate_id,
        changed = result.changed,
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
