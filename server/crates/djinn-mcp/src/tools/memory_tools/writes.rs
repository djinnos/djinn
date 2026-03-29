use super::*;

use crate::tools::memory_tools::contradiction::ContradictionAnalysisInput;
use crate::tools::memory_tools::summaries::NoteSummaryService;
use djinn_core::events::DjinnEventEnvelope;
use djinn_core::models::{Note, NoteDedupCandidate};
use djinn_db::note_hash::note_content_hash;
use djinn_db::{folder_for_type, is_singleton};
use djinn_provider::{CompletionRequest, CompletionResponse, complete, provider::LlmProvider};

const DEDUP_CANDIDATE_LIMIT: usize = 5;
const DEDUP_SYSTEM: &str = "You decide whether an incoming memory note should be skipped, merged into an existing note, or kept as a separate note. Respond with JSON only.";
const DEDUP_MAX_TOKENS: u32 = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DedupDisposition {
    Skip,
    Merge,
    KeepBoth,
}

#[derive(Clone, Debug)]
pub(crate) struct DedupDecision {
    pub disposition: DedupDisposition,
}

#[derive(serde::Deserialize)]
struct DedupDecisionPayload {
    decision: String,
}

/// Local decision seam for deduplication branching.
/// Tests can inject custom implementations to simulate skip|merge|keep_both decisions.
pub(crate) trait MemoryWriteDedupDecider: Send + Sync {
    fn decide<'a>(
        &'a self,
        project_path: &'a str,
        incoming_title: &'a str,
        incoming_content: &'a str,
        note_type: &'a str,
        candidates: &'a [NoteDedupCandidate],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DedupDecision, String>> + Send + 'a>,
    >;
}

struct LlmMemoryWriteDedupDecider {
    db: djinn_db::Database,
    runtime: MemoryWriteProviderRuntime,
}

#[derive(Clone)]
#[allow(clippy::type_complexity)]
struct MemoryWriteProviderRuntime {
    resolve_provider: std::sync::Arc<
        dyn for<'a> Fn(
                &'a djinn_db::Database,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Box<dyn LlmProvider>, String>>
                        + Send
                        + 'a,
                >,
            > + Send
            + Sync,
    >,
    complete: std::sync::Arc<
        dyn for<'a> Fn(
                &'a dyn LlmProvider,
                CompletionRequest,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<CompletionResponse, String>>
                        + Send
                        + 'a,
                >,
            > + Send
            + Sync,
    >,
}

impl Default for MemoryWriteProviderRuntime {
    fn default() -> Self {
        Self {
            resolve_provider: std::sync::Arc::new(|db| {
                let db = db.clone();
                Box::pin(async move {
                    djinn_provider::resolve_memory_provider(&db)
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
            complete: std::sync::Arc::new(|provider, request| {
                Box::pin(async move {
                    complete(provider, request)
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
        }
    }
}

impl LlmMemoryWriteDedupDecider {
    fn new(db: djinn_db::Database) -> Self {
        Self {
            db,
            runtime: MemoryWriteProviderRuntime::default(),
        }
    }

    #[cfg(test)]
    fn with_runtime(db: djinn_db::Database, runtime: MemoryWriteProviderRuntime) -> Self {
        Self { db, runtime }
    }
}

impl MemoryWriteDedupDecider for LlmMemoryWriteDedupDecider {
    fn decide<'a>(
        &'a self,
        project_path: &'a str,
        incoming_title: &'a str,
        incoming_content: &'a str,
        note_type: &'a str,
        candidates: &'a [NoteDedupCandidate],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DedupDecision, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let provider = (self.runtime.resolve_provider)(&self.db)
                .await
                .map_err(|error| error.to_string())?;
            let prompt = render_dedup_prompt(
                project_path,
                incoming_title,
                incoming_content,
                note_type,
                candidates,
            );
            let response = (self.runtime.complete)(
                provider.as_ref(),
                CompletionRequest {
                    system: DEDUP_SYSTEM.to_string(),
                    prompt,
                    max_tokens: DEDUP_MAX_TOKENS,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
            parse_dedup_decision(&response)
        })
    }
}

/// Returns true for note types that can be merged during deduplication.
///
/// Mergeable types benefit from consolidation.
/// Non-mergeable types (including singletons like brief, roadmap) get separate notes.
pub(crate) fn mergeable_note_type(note_type: &str) -> bool {
    matches!(
        note_type,
        "adr"
            | "pattern"
            | "case"
            | "pitfall"
            | "requirement"
            | "reference"
            | "design"
            | "session"
            | "persona"
            | "journey"
            | "design_spec"
            | "competitive"
            | "tech_spike"
    )
}

fn render_dedup_prompt(
    project_path: &str,
    incoming_title: &str,
    incoming_content: &str,
    note_type: &str,
    candidates: &[NoteDedupCandidate],
) -> String {
    let candidate_lines = candidates
        .iter()
        .map(|candidate| {
            format!(
                "- id: {}\n  permalink: {}\n  title: {}\n  score: {}\n  abstract: {}\n  overview: {}",
                candidate.id,
                candidate.permalink,
                candidate.title,
                candidate.score,
                candidate.abstract_.as_deref().unwrap_or(""),
                candidate.overview.as_deref().unwrap_or(""),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Project: {project_path}\nNote type: {note_type}\nIncoming title: {incoming_title}\nIncoming content:\n{incoming_content}\n\nCandidates:\n{candidate_lines}\n\nReturn JSON only with {{\"decision\":\"skip|merge|keep_both\"}}."
    )
}

fn parse_dedup_decision(response: &CompletionResponse) -> Result<DedupDecision, String> {
    let payload: DedupDecisionPayload =
        serde_json::from_str(response.text.trim()).map_err(|error| error.to_string())?;
    let disposition = match payload.decision.trim() {
        "skip" => DedupDisposition::Skip,
        "merge" => DedupDisposition::Merge,
        "keep_both" => DedupDisposition::KeepBoth,
        other => return Err(format!("invalid dedup decision: {other}")),
    };
    Ok(DedupDecision { disposition })
}

async fn dedup_candidates_for_write(
    repo: &NoteRepository,
    project_id: &str,
    note_type: &str,
    content: &str,
) -> Result<Vec<NoteDedupCandidate>, String> {
    let folder = folder_for_type(note_type);
    repo.dedup_candidates(
        project_id,
        folder,
        note_type,
        content,
        DEDUP_CANDIDATE_LIMIT,
    )
    .await
    .map_err(|error| error.to_string())
}

async fn exact_content_hash_match_for_write(
    repo: &NoteRepository,
    project_id: &str,
    content: &str,
) -> Option<Note> {
    let content_hash = note_content_hash(content);
    repo.find_by_content_hash(project_id, &content_hash)
        .await
        .ok()
        .flatten()
}

struct PendingWriteDedup<'a> {
    project_path: &'a str,
    project_id: &'a str,
    title: &'a str,
    content: &'a str,
    note_type: &'a str,
    tags_json: &'a str,
}

/// Apply deduplication logic to a pending write.
///
/// - Returns `Some(response)` on Skip (returns existing note) or Merge (updates existing note)
/// - Returns `None` on KeepBoth or when no candidates exist (caller should create new note)
async fn maybe_apply_write_dedup(
    repo: &NoteRepository,
    decider: &dyn MemoryWriteDedupDecider,
    pending: PendingWriteDedup<'_>,
) -> Option<MemoryNoteResponse> {
    if let Some(existing) =
        exact_content_hash_match_for_write(repo, pending.project_id, pending.content).await
    {
        return Some(MemoryNoteResponse::deduplicated_from_note(&existing));
    }

    // Bypass LLM/BM25 dedup for non-mergeable note types after exact-hash reuse.
    if !mergeable_note_type(pending.note_type) {
        return None;
    }

    let candidates =
        dedup_candidates_for_write(repo, pending.project_id, pending.note_type, pending.content)
            .await
            .ok()?;

    // No candidates above threshold; proceed with normal write
    if candidates.is_empty() {
        return None;
    }

    let decision = match decider
        .decide(
            pending.project_path,
            pending.title,
            pending.content,
            pending.note_type,
            &candidates,
        )
        .await
    {
        Ok(decision) => decision,
        // Errors fall back to keep_both (create new note)
        Err(_) => return None,
    };

    let existing_candidate = candidates.first()?;
    let existing = match repo.get(&existing_candidate.id).await {
        Ok(Some(note)) => note,
        _ => return None,
    };

    match decision.disposition {
        DedupDisposition::Skip => Some(MemoryNoteResponse::deduplicated_from_note(&existing)),
        DedupDisposition::Merge => {
            let merged_content = if existing.content.trim().is_empty() {
                pending.content.to_string()
            } else if pending.content.trim().is_empty() {
                existing.content.clone()
            } else {
                format!("{}\n\n{}", existing.content, pending.content)
            };
            match repo
                .update(
                    &existing.id,
                    &existing.title,
                    &merged_content,
                    pending.tags_json,
                )
                .await
            {
                Ok(note) => Some(MemoryNoteResponse::deduplicated_from_note(&note)),
                Err(_) => None,
            }
        }
        DedupDisposition::KeepBoth => None,
    }
}

#[tool_router(router = memory_writes_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Create or update a note. Type is required and determines storage folder.
    /// Singleton types (brief, roadmap) write a fixed file — one per project.
    #[tool(
        description = "Create or update a note. Type is required and determines storage folder (adr->decisions/, pattern->patterns/, case->cases/, pitfall->pitfalls/, research->research/, requirement->requirements/, reference->reference/, design->design/, persona->design/personas, journey->design/journeys, design_spec->design/specs, session->research/sessions, competitive->research/competitive, tech_spike->research/technical). Singleton types (brief, roadmap) write a fixed file at docs root — one per project, title is ignored. Use [[wikilinks]] in content to connect notes — any [[Note Title]] creates a link in the knowledge graph. Add a '## Relations' section at the bottom with '- [[Related Note]]' entries to make connections explicit. For large documents (>150 lines): create with initial content, then use memory_edit with operation=\"append\" to add remaining sections."
    )]
    pub async fn memory_write(
        &self,
        Parameters(p): Parameters<WriteParams>,
    ) -> Json<MemoryNoteResponse> {
        self.memory_write_with_worktree(Parameters(p), None).await
    }

    pub async fn memory_write_with_worktree(
        &self,
        Parameters(p): Parameters<WriteParams>,
        worktree_root: Option<std::path::PathBuf>,
    ) -> Json<MemoryNoteResponse> {
        self.memory_write_with_worktree_and_decider(
            Parameters(p),
            worktree_root,
            &LlmMemoryWriteDedupDecider::new(self.state.db().clone()),
        )
        .await
    }

    async fn memory_write_with_worktree_and_decider(
        &self,
        Parameters(p): Parameters<WriteParams>,
        worktree_root: Option<std::path::PathBuf>,
        decider: &dyn MemoryWriteDedupDecider,
    ) -> Json<MemoryNoteResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(MemoryNoteResponse::error(e)),
        };

        let tags_json = p
            .tags
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| "[]".to_string());

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus())
            .with_worktree_root(worktree_root);

        if is_singleton(&p.note_type)
            && let Some(existing) = repo
                .get_by_permalink(&project_id, &p.note_type)
                .await
                .ok()
                .flatten()
        {
            match repo
                .update(&existing.id, &p.title, &p.content, &tags_json)
                .await
            {
                Ok(note) => {
                    self.schedule_summary_regeneration(&note.id);
                    return Json(MemoryNoteResponse::from_note(&note));
                }
                Err(e) => return Json(MemoryNoteResponse::error(e.to_string())),
            }
        }

        // Deduplication flow: only for mergeable note types
        if let Some(response) = maybe_apply_write_dedup(
            &repo,
            decider,
            PendingWriteDedup {
                project_path: &p.project,
                project_id: &project_id,
                title: &p.title,
                content: &p.content,
                note_type: &p.note_type,
                tags_json: &tags_json,
            },
        )
        .await
        {
            if let Some(note_id) = response.id.as_deref()
                && response.error.is_none()
            {
                self.schedule_summary_regeneration(note_id);
            }
            return Json(response);
        }

        match repo
            .create(
                &project_id,
                Path::new(&p.project),
                &p.title,
                &p.content,
                &p.note_type,
                &tags_json,
            )
            .await
        {
            Ok(note) => {
                self.schedule_summary_regeneration(&note.id);
                self.detect_emit_and_schedule_contradictions(&repo, &note)
                    .await;
                Json(MemoryNoteResponse::from_note(&note))
            }
            Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
        }
    }

    /// Edit an existing note. Operations: "append" (add to end), "prepend" (add
    /// after frontmatter), "find_replace" (exact text replacement, requires
    /// find_text), "replace_section" (replace content under a markdown heading,
    /// requires section). Use append to build large notes incrementally after
    /// memory_write creates the initial note. When type is provided and differs
    /// from current type, the note is automatically moved to the correct folder
    /// for the new type.
    #[tool(
        description = "Edit an existing note. Operations: \"append\" (add to end), \"prepend\" (add after frontmatter), \"find_replace\" (exact text replacement, requires find_text), \"replace_section\" (replace content under a markdown heading, requires section). Use append to build large notes incrementally after memory_write creates the initial note. When type is provided and differs from current type, the note is automatically moved to the correct folder for the new type."
    )]
    pub async fn memory_edit(
        &self,
        Parameters(p): Parameters<EditParams>,
    ) -> Json<MemoryNoteResponse> {
        self.memory_edit_with_worktree(Parameters(p), None).await
    }

    pub async fn memory_edit_with_worktree(
        &self,
        Parameters(p): Parameters<EditParams>,
        worktree_root: Option<std::path::PathBuf>,
    ) -> Json<MemoryNoteResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryNoteResponse::error(format!(
                "project not found: {}",
                p.project
            )));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus())
            .with_worktree_root(worktree_root);

        let note = match resolve_note_by_identifier(&repo, &project_id, &p.identifier).await {
            Some(n) => n,
            None => {
                return Json(MemoryNoteResponse::error(format!(
                    "note not found: {}",
                    p.identifier
                )));
            }
        };

        let note = if let Some(ref new_type) = p.note_type {
            if new_type != &note.note_type {
                match repo
                    .move_note(&note.id, Path::new(&p.project), &note.title, new_type)
                    .await
                {
                    Ok(moved) => moved,
                    Err(e) => return Json(MemoryNoteResponse::error(e.to_string())),
                }
            } else {
                note
            }
        } else {
            note
        };

        let new_content = match apply_edit_operation(
            &note.content,
            &p.operation,
            &p.content,
            p.find_text.as_deref(),
            p.section.as_deref(),
        ) {
            Ok(c) => c,
            Err(e) => return Json(MemoryNoteResponse::error(e)),
        };

        match repo
            .update(&note.id, &note.title, &new_content, &note.tags)
            .await
        {
            Ok(updated) => {
                self.schedule_summary_regeneration(&updated.id);
                Json(MemoryNoteResponse::from_note(&updated))
            }
            Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
        }
    }

    /// Delete a note. Removes file and index entry.
    #[tool(description = "Delete a note. Removes file and index entry.")]
    pub async fn memory_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Json<MemoryDeleteResponse> {
        self.memory_delete_with_worktree(Parameters(p), None).await
    }

    pub(crate) async fn memory_delete_with_worktree(
        &self,
        Parameters(p): Parameters<DeleteParams>,
        worktree_root: Option<std::path::PathBuf>,
    ) -> Json<MemoryDeleteResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryDeleteResponse {
                ok: false,
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus())
            .with_worktree_root(worktree_root);

        let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
            return Json(MemoryDeleteResponse {
                ok: false,
                error: Some(format!("note not found: {}", p.identifier)),
            });
        };

        match repo.delete(&note.id).await {
            Ok(()) => Json(MemoryDeleteResponse {
                ok: true,
                error: None,
            }),
            Err(e) => Json(MemoryDeleteResponse {
                ok: false,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Move a note to a new location. Updates permalink and resolves inbound links.
    #[tool(
        description = "Move a note to a new location. Updates permalink and resolves inbound links."
    )]
    pub async fn memory_move(
        &self,
        Parameters(p): Parameters<MoveParams>,
    ) -> Json<MemoryNoteResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryNoteResponse::error(format!(
                "project not found: {}",
                p.project
            )));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
            return Json(MemoryNoteResponse::error(format!(
                "note not found: {}",
                p.identifier
            )));
        };

        let new_title = p.title.as_deref().unwrap_or(&note.title);
        let moved_title = p.title.as_deref().unwrap_or(&note.title);

        match repo
            .move_note(&note.id, Path::new(&p.project), moved_title, &p.note_type)
            .await
        {
            Ok(mut moved) => {
                if p.title.is_some() {
                    moved.title = new_title.to_string();
                }
                Json(MemoryNoteResponse::from_note(&moved))
            }
            Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
        }
    }
}

impl DjinnMcpServer {
    fn schedule_summary_regeneration(&self, note_id: &str) {
        let db = self.state.db().clone();
        let note_id = note_id.to_string();
        tokio::spawn(async move {
            let service = NoteSummaryService::new(db.clone());
            match djinn_provider::resolve_memory_provider(&db).await {
                Ok(_) => service.generate_for_note_ids(&[note_id]).await,
                Err(_) => service.apply_fallback_for_note_id(&note_id).await,
            }
        });
    }

    /// Stage 1: detect candidates and emit event. Stage 2: send to analysis worker.
    ///
    /// The analysis worker is triggered only when stage 1 finds candidates and emits
    /// the `contradiction_candidates` event — never on every write.
    async fn detect_emit_and_schedule_contradictions(&self, repo: &NoteRepository, note: &Note) {
        let folder = folder_for_type(&note.note_type);
        let Ok(candidates) = repo
            .detect_contradiction_candidates(&note.id, &note.note_type, folder, &note.content)
            .await
        else {
            return;
        };

        if candidates.is_empty() {
            return;
        }

        // Stage 1: emit event for SSE / external listeners
        self.state
            .event_bus()
            .send(DjinnEventEnvelope::contradiction_candidates(
                note,
                &candidates,
            ));

        // Stage 2: send to the contradiction analysis worker channel.
        // The worker is only active when stage 1 emits candidates — satisfying the
        // requirement that LLM analysis is triggered by the contradiction_candidates event.
        let input = ContradictionAnalysisInput {
            note_id: note.id.clone(),
            note_title: note.title.clone(),
            note_summary: note
                .abstract_
                .clone()
                .unwrap_or_else(|| note.content.chars().take(500).collect()),
            candidates,
        };
        let _ = self.contradiction_analysis_tx.try_send(input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::DjinnMcpServer, state::stubs::test_mcp_state};
    use djinn_core::events::EventBus;
    use djinn_core::message::{ContentBlock, Conversation};
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use djinn_provider::provider::ToolChoice;
    use futures::Stream;
    use futures::stream;
    use rmcp::{Json, handler::server::wrapper::Parameters};
    use serde_json::Value;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct StubDedupDecider {
        result: Result<DedupDecision, String>,
    }

    impl MemoryWriteDedupDecider for StubDedupDecider {
        fn decide<'a>(
            &'a self,
            _project_path: &'a str,
            _incoming_title: &'a str,
            _incoming_content: &'a str,
            _note_type: &'a str,
            _candidates: &'a [NoteDedupCandidate],
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<DedupDecision, String>> + Send + 'a>,
        > {
            Box::pin(async move { self.result.clone() })
        }
    }

    async fn create_project(db: &Database, root: &std::path::Path) -> djinn_core::models::Project {
        ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", root.to_str().unwrap())
            .await
            .unwrap()
    }

    async fn write_with_decider(
        server: &DjinnMcpServer,
        params: WriteParams,
        decider: &dyn MemoryWriteDedupDecider,
    ) -> MemoryNoteResponse {
        let Json(response) = server
            .memory_write_with_worktree_and_decider(Parameters(params), None, decider)
            .await;
        response
    }

    async fn note_count_for_project(db: &Database, project_id: &str) -> usize {
        NoteRepository::new(db.clone(), EventBus::noop())
            .list(project_id, None)
            .await
            .unwrap()
            .len()
    }

    struct MockProvider {
        response_text: String,
        stream_calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(response_text: impl Into<String>) -> Self {
            Self {
                response_text: response_text.into(),
                stream_calls: AtomicUsize::new(0),
            }
        }
    }

    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock-memory-write-provider"
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [Value],
            _tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<
                                    dyn Stream<
                                            Item = anyhow::Result<
                                                djinn_provider::provider::StreamEvent,
                                            >,
                                        > + Send,
                                >,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            let response_text = self.response_text.clone();
            Box::pin(async move {
                let events: Vec<anyhow::Result<djinn_provider::provider::StreamEvent>> = vec![
                    Ok(djinn_provider::provider::StreamEvent::Delta(
                        ContentBlock::text(response_text),
                    )),
                    Ok(djinn_provider::provider::StreamEvent::Done),
                ];
                Ok(Box::pin(stream::iter(events))
                    as Pin<
                        Box<
                            dyn Stream<Item = anyhow::Result<djinn_provider::provider::StreamEvent>>
                                + Send,
                        >,
                    >)
            })
        }
    }

    fn provider_runtime_with_response(response_text: &'static str) -> MemoryWriteProviderRuntime {
        MemoryWriteProviderRuntime {
            resolve_provider: std::sync::Arc::new(move |_db| {
                Box::pin(async move {
                    Ok(Box::new(MockProvider::new(response_text)) as Box<dyn LlmProvider>)
                })
            }),
            complete: std::sync::Arc::new(|provider, request| {
                Box::pin(async move {
                    complete(provider, request)
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
        }
    }

    fn provider_runtime_with_resolution_error(message: &'static str) -> MemoryWriteProviderRuntime {
        MemoryWriteProviderRuntime {
            resolve_provider: std::sync::Arc::new(move |_db| {
                Box::pin(async move { Err(message.to_string()) })
            }),
            complete: std::sync::Arc::new(|provider, request| {
                Box::pin(async move {
                    complete(provider, request)
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
        }
    }

    async fn write_with_provider_runtime(
        server: &DjinnMcpServer,
        db: &Database,
        params: WriteParams,
        runtime: MemoryWriteProviderRuntime,
    ) -> MemoryNoteResponse {
        let decider = LlmMemoryWriteDedupDecider::with_runtime(db.clone(), runtime);
        write_with_decider(server, params, &decider).await
    }

    async fn create_indexed_note(
        server: &DjinnMcpServer,
        project_path: &std::path::Path,
        title: &str,
        content: &str,
        note_type: &str,
    ) -> MemoryNoteResponse {
        let Json(response) = server
            .memory_write(Parameters(WriteParams {
                project: project_path.to_str().unwrap().to_string(),
                title: title.to_string(),
                content: content.to_string(),
                note_type: note_type.to_string(),
                tags: None,
            }))
            .await;
        response
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_bypasses_dedup_for_non_mergeable_types() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let decider = StubDedupDecider {
            result: Err("should not be called".to_string()),
        };

        let response = write_with_decider(
            &server,
            WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Brief".to_string(),
                content: "singleton body".to_string(),
                note_type: "brief".to_string(),
                tags: None,
            },
            &decider,
        )
        .await;

        assert!(response.error.is_none());
        assert_eq!(response.note_type.as_deref(), Some("brief"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_creates_new_note_when_no_dedup_candidates_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let _project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let decider = StubDedupDecider {
            result: Ok(DedupDecision {
                disposition: DedupDisposition::Skip,
            }),
        };

        let response = write_with_decider(
            &server,
            WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Fresh Reference".to_string(),
                content: "unique phrase here".to_string(),
                note_type: "reference".to_string(),
                tags: None,
            },
            &decider,
        )
        .await;

        assert!(response.error.is_none());
        assert_eq!(response.title.as_deref(), Some("Fresh Reference"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_provider_backed_skip_returns_existing_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let existing = create_indexed_note(
            &server,
            tmp.path(),
            "Existing Pattern",
            "tokio spawn concurrent execution pattern for rust services",
            "pattern",
        )
        .await;
        assert!(existing.error.is_none());

        let response = write_with_provider_runtime(
            &server,
            &db,
            WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Incoming Pattern".to_string(),
                content: "tokio spawn concurrent execution pattern for rust services".to_string(),
                note_type: "pattern".to_string(),
                tags: None,
            },
            provider_runtime_with_response(r#"{"decision":"skip"}"#),
        )
        .await;

        assert!(response.error.is_none());
        assert!(response.id.is_some());
        assert_eq!(response.id, existing.id);
        assert_eq!(note_count_for_project(&db, &project.id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_provider_backed_merge_updates_existing_note() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        // Use identical content to ensure FTS finds it as a dedup candidate
        let shared_content = "rust retry backoff strategy for transient service calls with exponential backoff and jitter";
        let existing = create_indexed_note(
            &server,
            tmp.path(),
            "Merge Target",
            shared_content,
            "pattern",
        )
        .await;
        assert!(existing.error.is_none());
        let existing_id = existing.id.clone();

        // Use identical content to ensure high FTS match score
        let response = write_with_provider_runtime(
            &server,
            &db,
            WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Incoming Note".to_string(),
                content: shared_content.to_string(),
                note_type: "pattern".to_string(),
                tags: None,
            },
            provider_runtime_with_response(r#"{"decision":"merge"}"#),
        )
        .await;

        assert!(
            response.error.is_none(),
            "Expected no error but got: {:?}",
            response.error
        );
        assert!(response.id.is_some(), "Expected response to have an id");
        assert_eq!(
            response.id.as_deref(),
            existing_id.as_deref(),
            "Expected merged note to have same id as existing"
        );
        assert_eq!(
            note_count_for_project(&db, &project.id).await,
            1,
            "Expected only 1 note after merge"
        );

        let merged = repo
            .get(existing_id.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(merged.content.contains("rust retry backoff strategy"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_write_provider_resolution_failures_fall_back_to_keep_both() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let project = create_project(&db, tmp.path()).await;
        let server = DjinnMcpServer::new(state);

        let existing = create_indexed_note(
            &server,
            tmp.path(),
            "Existing Note",
            "database migration sequencing playbook for releases",
            "pattern",
        )
        .await;
        assert!(existing.error.is_none());

        let response = write_with_provider_runtime(
            &server,
            &db,
            WriteParams {
                project: tmp.path().to_str().unwrap().to_string(),
                title: "Incoming Fallback".to_string(),
                content:
                    "database migration sequencing playbook for releases with rollback checklist"
                        .to_string(),
                note_type: "pattern".to_string(),
                tags: None,
            },
            provider_runtime_with_resolution_error("provider unavailable"),
        )
        .await;

        assert!(response.error.is_none());
        assert_ne!(response.id, existing.id);
        assert_eq!(note_count_for_project(&db, &project.id).await, 2);
    }

    #[test]
    fn parse_dedup_decision_rejects_invalid_json_and_invalid_decision() {
        assert!(
            parse_dedup_decision(&CompletionResponse {
                text: "not json".to_string(),
                ..CompletionResponse::default()
            })
            .is_err()
        );
        assert!(
            parse_dedup_decision(&CompletionResponse {
                text: r#"{"decision":"other"}"#.to_string(),
                ..CompletionResponse::default()
            })
            .is_err()
        );
    }

    #[test]
    fn mergeable_note_type_bypasses_singletons() {
        assert!(mergeable_note_type("reference"));
        assert!(!mergeable_note_type("brief"));
        assert!(!mergeable_note_type("roadmap"));
    }
}
