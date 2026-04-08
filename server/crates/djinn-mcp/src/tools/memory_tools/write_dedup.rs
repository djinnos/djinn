use async_trait::async_trait;
use djinn_core::models::{Note, NoteDedupCandidate};
use djinn_db::{NoteRepository, folder_for_type, note_hash::note_content_hash};
use djinn_provider::CompletionRequest;

use super::MemoryNoteResponse;
use super::write_dedup_prompt::{
    MEMORY_WRITE_DEDUP_SYSTEM, parse_memory_write_dedup_decision, render_memory_write_dedup_prompt,
};
use super::write_dedup_runtime::{LlmMemoryWriteProviderRuntime, MemoryWriteProviderRuntime};

const MEMORY_WRITE_DEDUP_MAX_TOKENS: u32 = 768;
const MEMORY_WRITE_DEDUP_CANDIDATE_LIMIT: usize = 5;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingWriteDedup<'a> {
    pub(crate) project_path: &'a str,
    pub(crate) project_id: &'a str,
    pub(crate) title: &'a str,
    pub(crate) content: &'a str,
    pub(crate) note_type: &'a str,
    pub(crate) tags_json: &'a str,
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryWriteDedupDecisionInput<'a> {
    pub(crate) project_path: &'a str,
    pub(crate) title: &'a str,
    pub(crate) content: &'a str,
    pub(crate) note_type: &'a str,
    pub(crate) candidates: &'a [NoteDedupCandidate],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryWriteDedupDecision {
    CreateNew,
    ReuseExisting {
        candidate_id: String,
    },
    MergeIntoExisting {
        candidate_id: String,
        merged_title: String,
        merged_content: String,
    },
}

#[async_trait]
pub(crate) trait MemoryWriteDedupDecider: Send + Sync {
    async fn decide(
        &self,
        input: MemoryWriteDedupDecisionInput<'_>,
    ) -> Result<MemoryWriteDedupDecision, String>;
}

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

#[async_trait]
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
    let folder = folder_for_type(pending.note_type);
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

async fn apply_dedup_decision(
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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProjectRepository};
    use djinn_provider::CompletionResponse;

    struct StaticDecider {
        decision: MemoryWriteDedupDecision,
    }

    #[async_trait]
    impl MemoryWriteDedupDecider for StaticDecider {
        async fn decide(
            &self,
            _input: MemoryWriteDedupDecisionInput<'_>,
        ) -> Result<MemoryWriteDedupDecision, String> {
            Ok(self.decision.clone())
        }
    }

    struct StaticRuntime {
        text: String,
    }

    #[async_trait]
    impl MemoryWriteProviderRuntime for StaticRuntime {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, String> {
            Ok(CompletionResponse {
                text: self.text.clone(),
                ..CompletionResponse::default()
            })
        }
    }

    async fn create_project(db: &Database, root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", root.to_str().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn exact_hash_match_short_circuits_decider() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let existing = repo
            .create(
                &project.id,
                tmp.path(),
                "Canonical",
                "Alpha\r\nBeta\n",
                "research",
                "[]",
            )
            .await
            .unwrap();

        let response = maybe_apply_write_dedup(
            &repo,
            &StaticDecider {
                decision: MemoryWriteDedupDecision::CreateNew,
            },
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "Duplicate",
                content: "  Alpha\nBeta  ",
                note_type: "research",
                tags_json: "[]",
            },
        )
        .await
        .unwrap();

        assert_eq!(response.id.as_deref(), Some(existing.id.as_str()));
        assert!(response.deduplicated);
    }

    #[tokio::test]
    async fn llm_decider_can_merge_existing_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = create_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let existing = repo
            .create(
                &project.id,
                tmp.path(),
                "Async Pattern",
                "tokio spawn",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let response = apply_dedup_decision(
            &repo,
            PendingWriteDedup {
                project_path: tmp.path().to_str().unwrap(),
                project_id: &project.id,
                title: "Async Pattern Updated",
                content: "tokio spawn joinset",
                note_type: "pattern",
                tags_json: "[]",
            },
            MemoryWriteDedupDecision::MergeIntoExisting {
                candidate_id: existing.id.clone(),
                merged_title: "Async Pattern".to_string(),
                merged_content: "tokio spawn\njoinset".to_string(),
            },
        )
        .await
        .unwrap()
        .unwrap();

        let updated = repo.get(&existing.id).await.unwrap().unwrap();
        assert_eq!(response.id.as_deref(), Some(existing.id.as_str()));
        assert_eq!(updated.content, "tokio spawn\njoinset");
    }

    #[tokio::test]
    async fn llm_decider_parses_runtime_response() {
        let decider = LlmMemoryWriteDedupDecider::with_runtime(Box::new(StaticRuntime {
            text: r#"{"action":"reuse_existing","candidate_id":"note_1"}"#.to_string(),
        }));

        let decision = decider
            .decide(MemoryWriteDedupDecisionInput {
                project_path: "/tmp/project",
                title: "Title",
                content: "Body",
                note_type: "pattern",
                candidates: &[],
            })
            .await
            .unwrap();

        assert_eq!(
            decision,
            MemoryWriteDedupDecision::ReuseExisting {
                candidate_id: "note_1".to_string()
            }
        );
    }
}
