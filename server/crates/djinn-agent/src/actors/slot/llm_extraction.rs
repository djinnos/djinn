//! LLM-powered knowledge extraction from completed sessions.
//!
//! After structural extraction builds the `SessionTaxonomy`, this module feeds
//! the taxonomy + task description to an LLM and extracts three note types:
//!
//! - **cases**: problem + solution pairs from successful task outcomes
//! - **patterns**: reusable processes or methods discovered during the session
//! - **pitfalls**: errors encountered and how they were resolved
//!
//! Each extracted note goes through the normal note-creation pipeline. Notes
//! start at confidence 0.5 (lower than human-written 1.0). Session provenance
//! is recorded in the note content footer.
//!
//! All errors are logged as warnings; nothing propagates to the caller.

use std::sync::Arc;

use djinn_db::{
    NoteRepository, ProjectRepository, SessionRepository, TaskRepository, folder_for_type,
};
use djinn_provider::provider::LlmProvider;
use djinn_provider::{CompletionRequest, complete, resolve_memory_provider};
use serde::Deserialize;

use super::session_extraction::SessionTaxonomy;
use crate::context::AgentContext;

// ── Prompt constants ──────────────────────────────────────────────────────────

const SYSTEM_PROMPT: &str = "You are a knowledge extractor. Given a completed agent session \
summary, extract reusable knowledge as structured notes. Respond with valid JSON only.";

/// Maximum novelty candidates to check before creating a new note.
const NOVELTY_CANDIDATE_LIMIT: usize = 3;

/// Confidence signal applied to an existing note when a new extraction is
/// semantically judged to be already known.
const DUPLICATE_CONFIDENCE_SIGNAL: f64 = 0.65;

const EXTRACTION_SYSTEM_PROMPT: &str = SYSTEM_PROMPT;
const NOVELTY_SYSTEM_PROMPT: &str = "You are a semantic novelty judge for extracted knowledge notes. Compare a proposed note summary against an existing note summary. Respond with valid JSON only.";

const MIN_DURABLE_WORDS: usize = 16;

// ── JSON response shape ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct ExtractedNote {
    title: String,
    content: String,
    #[serde(default)]
    scope_paths: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ExtractionResponse {
    #[serde(default)]
    cases: Vec<ExtractedNote>,
    #[serde(default)]
    patterns: Vec<ExtractedNote>,
    #[serde(default)]
    pitfalls: Vec<ExtractedNote>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NoveltyDecisionKind {
    AlreadyKnown,
    Novel,
}

#[derive(Debug, Deserialize)]
struct NoveltyDecision {
    decision: NoveltyDecisionKind,
    existing_note_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum ExtractionOutcome {
    DurableWrite,
    MergeIntoExisting,
    DowngradeToWorkingSpec,
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoveltyAssessment {
    Novel,
    Duplicate,
    Unknown,
}

#[derive(Debug, Clone)]
struct QualityAssessment {
    specificity: bool,
    generality: bool,
    durability: bool,
    novelty: NoveltyAssessment,
    type_fit: bool,
    outcome: ExtractionOutcome,
    reasons: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct NoveltyCheckResult {
    assessment: NoveltyAssessment,
    existing_note_id: Option<String>,
}

#[cfg(test)]
type CandidateLookupOverride = fn(&str, &str, &str, &str) -> Vec<djinn_db::NoteDedupCandidate>;

struct ExtractionContext<'a> {
    note_repo: &'a NoteRepository,
    provider: &'a dyn LlmProvider,
    project_id: &'a str,
    session_id: &'a str,
    provenance: &'a str,
    session_scope_paths: &'a [String],
    #[cfg(test)]
    candidate_lookup: CandidateLookup,
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct CandidateLookup {
    override_lookup: Option<CandidateLookupOverride>,
}

#[cfg(test)]
impl CandidateLookup {
    const fn production() -> Self {
        Self {
            override_lookup: None,
        }
    }

    const fn with_override(override_lookup: CandidateLookupOverride) -> Self {
        Self {
            override_lookup: Some(override_lookup),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run LLM-based knowledge extraction for a completed session.
///
/// Loads the session, resolves its task and project, calls the LLM to extract
/// structured notes, and writes each note via `NoteRepository::create`.
///
/// All errors are logged as warnings; nothing propagates to the caller.
pub(crate) async fn run_llm_extraction(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: AgentContext,
) {
    run_llm_extraction_inner(
        session_id,
        taxonomy,
        app_state,
        None,
        #[cfg(test)]
        None,
    )
    .await;
}

/// Test-only entry point that injects a pre-built LLM provider, bypassing
/// credential loading and `resolve_memory_provider`.
#[cfg(test)]
pub(crate) async fn run_llm_extraction_with_provider(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: AgentContext,
    provider: Arc<dyn LlmProvider>,
) {
    run_llm_extraction_inner(session_id, taxonomy, app_state, Some(provider), None).await;
}

#[cfg(test)]
pub(crate) async fn run_llm_extraction_with_provider_and_candidate_lookup(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: AgentContext,
    provider: Arc<dyn LlmProvider>,
    candidate_lookup_override: CandidateLookupOverride,
) {
    run_llm_extraction_inner(
        session_id,
        taxonomy,
        app_state,
        Some(provider),
        Some(candidate_lookup_override),
    )
    .await;
}

/// Inner implementation that accepts an optional provider override for test injection.
///
/// When `provider_override` is `Some`, the given provider is used directly
/// instead of calling `resolve_memory_provider`.
async fn run_llm_extraction_inner(
    session_id: String,
    mut taxonomy: SessionTaxonomy,
    app_state: AgentContext,
    provider_override: Option<Arc<dyn LlmProvider>>,
    #[cfg(test)] candidate_lookup_override: Option<CandidateLookupOverride>,
) {
    // ── Load session ───────────────────────────────────────────────────────
    let session_repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let session = match session_repo.get(&session_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::debug!(
                session_id = %session_id,
                "llm_extraction: session not found; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "llm_extraction: failed to load session; skipping"
            );
            return;
        }
    };

    // ── Require a task_id ─────────────────────────────────────────────────
    let task_id = match session.task_id {
        Some(ref id) => id.clone(),
        None => {
            tracing::debug!(
                session_id = %session_id,
                "llm_extraction: session has no task_id; skipping"
            );
            return;
        }
    };

    // ── Load task ──────────────────────────────────────────────────────────
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match task_repo.get(&task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::debug!(
                session_id = %session_id,
                task_id = %task_id,
                "llm_extraction: task not found; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                task_id = %task_id,
                error = %e,
                "llm_extraction: failed to load task; skipping"
            );
            return;
        }
    };

    // ── Load project ───────────────────────────────────────────────────────
    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let project = match project_repo.get(&session.project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::debug!(
                session_id = %session_id,
                project_id = %session.project_id,
                "llm_extraction: project not found; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                project_id = %session.project_id,
                error = %e,
                "llm_extraction: failed to load project; skipping"
            );
            return;
        }
    };

    // ── Resolve provider ───────────────────────────────────────────────────
    // In tests, a provider_override bypasses credential loading entirely.
    let provider: Box<dyn LlmProvider> = if let Some(p) = provider_override {
        struct ArcProvider(Arc<dyn LlmProvider>);
        use std::pin::Pin;
        impl LlmProvider for ArcProvider {
            fn name(&self) -> &str {
                self.0.name()
            }
            fn stream<'a>(
                &'a self,
                conv: &'a djinn_provider::message::Conversation,
                tools: &'a [serde_json::Value],
                tool_choice: Option<djinn_provider::provider::ToolChoice>,
            ) -> Pin<
                Box<
                    dyn futures::Future<
                            Output = anyhow::Result<
                                Pin<
                                    Box<
                                        dyn futures::Stream<
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
                self.0.stream(conv, tools, tool_choice)
            }
        }
        Box::new(ArcProvider(p))
    } else {
        match resolve_memory_provider(&app_state.db).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "llm_extraction: no LLM provider available; skipping extraction"
                );
                return;
            }
        }
    };

    // ── Build prompt ───────────────────────────────────────────────────────
    let taxonomy_json = serde_json::to_string(&taxonomy).unwrap_or_else(|_| "{}".to_string());
    let project_path = &project.path;
    let session_scope_paths = crate::actors::slot::session_extraction::derive_scope_paths(
        &taxonomy.changed_file_paths,
        project_path,
    );
    let scope_json =
        serde_json::to_string(&session_scope_paths).unwrap_or_else(|_| "[]".to_string());
    let prompt = format!(
        "Task: {title}\n\
         Description: {description}\n\n\
         Session event counts: {taxonomy_json}\n\n\
         Files touched were in these areas: {scope_json}\n\
         Include a \"scope_paths\" array per note with relevant path prefixes from the list above.\n\n\
         Extract knowledge from this session. Return JSON:\n\
         {{\n\
           \"cases\": [{{\"title\": \"...\", \"content\": \"Brief problem and solution description\", \"scope_paths\": [\"...\"]}}],\n\
           \"patterns\": [{{\"title\": \"...\", \"content\": \"Reusable process or method discovered\", \"scope_paths\": [\"...\"]}}],\n\
           \"pitfalls\": [{{\"title\": \"...\", \"content\": \"Error encountered and how it was resolved\", \"scope_paths\": [\"...\"]}}]\n\
         }}\n\
         Return empty arrays if nothing significant was learned. \
         Maximum 3 cases, 3 patterns, 2 pitfalls.\n\
         Only extract if there is clear signal (high errors+files_changed suggests pitfalls; \
         many notes_written suggests patterns).",
        title = task.title,
        description = task.description,
        taxonomy_json = taxonomy_json,
        scope_json = scope_json,
    );

    // ── Call LLM ───────────────────────────────────────────────────────────
    let response = match complete(
        provider.as_ref(),
        CompletionRequest {
            system: EXTRACTION_SYSTEM_PROMPT.to_string(),
            prompt,
            max_tokens: 1024,
        },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "llm_extraction: LLM completion failed; skipping extraction"
            );
            return;
        }
    };

    // ── Parse JSON response ────────────────────────────────────────────────
    let extracted = match parse_extraction_response(&response.text) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                raw_response = %response.text,
                "llm_extraction: failed to parse LLM response; skipping"
            );
            return;
        }
    };

    let total = extracted.cases.len() + extracted.patterns.len() + extracted.pitfalls.len();
    taxonomy.extraction_quality.extracted = total as u32;
    if total == 0 {
        persist_extraction_quality(&session_repo, &session_id, &taxonomy).await;
        tracing::debug!(
            session_id = %session_id,
            "llm_extraction: no notes extracted"
        );
        return;
    }

    tracing::debug!(
        session_id = %session_id,
        cases = extracted.cases.len(),
        patterns = extracted.patterns.len(),
        pitfalls = extracted.pitfalls.len(),
        "llm_extraction: writing extracted notes"
    );

    // ── Write notes ────────────────────────────────────────────────────────
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let provenance = format!(
        "\n\n---\n*Extracted from session {session_id}. Confidence: 0.5 (session-extracted).*"
    );

    let note_pairs: Vec<(&str, &[ExtractedNote])> = vec![
        ("case", extracted.cases.as_slice()),
        ("pattern", extracted.patterns.as_slice()),
        ("pitfall", extracted.pitfalls.as_slice()),
    ];

    let mut extraction_quality = taxonomy.extraction_quality.clone();
    let extraction_context = ExtractionContext {
        note_repo: &note_repo,
        provider: provider.as_ref(),
        project_id: &project.id,
        session_id: &session_id,
        provenance: &provenance,
        session_scope_paths: &session_scope_paths,
        #[cfg(test)]
        candidate_lookup: candidate_lookup_override
            .map(|lookup| CandidateLookup::with_override(lookup))
            .unwrap_or_else(CandidateLookup::production),
    };

    for (note_type, notes) in note_pairs {
        for note in notes {
            process_extracted_note(
                &extraction_context,
                note_type,
                note,
                &mut extraction_quality,
            )
            .await;
        }
    }

    taxonomy.extraction_quality = extraction_quality;

    persist_extraction_quality(&session_repo, &session_id, &taxonomy).await;
}

async fn persist_extraction_quality(
    session_repo: &SessionRepository,
    session_id: &str,
    taxonomy: &SessionTaxonomy,
) {
    let taxonomy_json = match serde_json::to_string(taxonomy) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "llm_extraction: failed to serialize taxonomy with extraction quality"
            );
            return;
        }
    };

    if let Err(error) = session_repo
        .set_event_taxonomy(session_id, &taxonomy_json)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "llm_extraction: failed to persist extraction quality taxonomy"
        );
    }
}

async fn process_extracted_note(
    extraction_context: &ExtractionContext<'_>,
    note_type: &str,
    note: &ExtractedNote,
    extraction_quality: &mut super::session_extraction::ExtractionQuality,
) {
    let novelty = match novelty_decision(extraction_context, note_type, note).await {
        Ok(result) => result,
        Err(e) => {
            tracing::debug!(
                session_id = %extraction_context.session_id,
                note_type = %note_type,
                title = %note.title,
                error = %e,
                "llm_extraction: novelty check failed; evaluating with unknown novelty"
            );
            NoveltyCheckResult {
                assessment: NoveltyAssessment::Unknown,
                existing_note_id: None,
            }
        }
    };

    let assessment = assess_quality_gate(note_type, note, &novelty);

    tracing::debug!(
        session_id = %extraction_context.session_id,
        note_type = %note_type,
        title = %note.title,
        outcome = ?assessment.outcome,
        specificity = assessment.specificity,
        generality = assessment.generality,
        durability = assessment.durability,
        novelty = ?assessment.novelty,
        type_fit = assessment.type_fit,
        reasons = ?assessment.reasons,
        "llm_extraction: evaluated extraction quality gate"
    );

    match assessment.outcome {
        ExtractionOutcome::MergeIntoExisting => {
            if let Some(candidate_id) = novelty.existing_note_id.as_deref() {
                match extraction_context
                    .note_repo
                    .update_confidence(candidate_id, DUPLICATE_CONFIDENCE_SIGNAL)
                    .await
                {
                    Ok(updated_confidence) => tracing::debug!(
                        session_id = %extraction_context.session_id,
                        note_type = %note_type,
                        title = %note.title,
                        existing_note_id = %candidate_id,
                        updated_confidence,
                        "llm_extraction: merged extraction into existing note via confidence boost"
                    ),
                    Err(e) => tracing::warn!(
                        session_id = %extraction_context.session_id,
                        note_type = %note_type,
                        title = %note.title,
                        existing_note_id = %candidate_id,
                        error = %e,
                        "llm_extraction: merge outcome failed to update existing confidence"
                    ),
                }
                extraction_quality.novelty_skipped += 1;
                extraction_quality.merged += 1;
            }
            return;
        }
        ExtractionOutcome::DowngradeToWorkingSpec => {
            extraction_quality.downgraded += 1;
            return;
        }
        ExtractionOutcome::Discard => {
            extraction_quality.discarded += 1;
            return;
        }
        ExtractionOutcome::DurableWrite => {}
    }

    let content_with_provenance = format!("{}{}", note.content, extraction_context.provenance);
    let scope_paths = if note.scope_paths.is_empty() {
        extraction_context.session_scope_paths.to_vec()
    } else {
        note.scope_paths.clone()
    };
    let scope_paths_json = serde_json::to_string(&scope_paths).unwrap_or_else(|_| "[]".to_string());
    match extraction_context
        .note_repo
        .create_db_note_with_scope(
            extraction_context.project_id,
            &note.title,
            &content_with_provenance,
            note_type,
            "[]",
            &scope_paths_json,
        )
        .await
    {
        Ok(created) => {
            if let Err(e) = extraction_context
                .note_repo
                .set_confidence(&created.id, 0.5)
                .await
            {
                tracing::warn!(
                    session_id = %extraction_context.session_id,
                    note_id = %created.id,
                    error = %e,
                    "llm_extraction: failed to set confidence on extracted note"
                );
            }
            tracing::debug!(
                session_id = %extraction_context.session_id,
                note_id = %created.id,
                note_type = %note_type,
                title = %note.title,
                "llm_extraction: note created"
            );
            extraction_quality.written += 1;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %extraction_context.session_id,
                note_type = %note_type,
                title = %note.title,
                error = %e,
                "llm_extraction: failed to create note; skipping"
            );
        }
    }
}

async fn novelty_decision(
    extraction_context: &ExtractionContext<'_>,
    note_type: &str,
    note: &ExtractedNote,
) -> Result<NoveltyCheckResult, String> {
    let candidate_abstract = summarize_candidate_note(note);
    let folder = folder_for_type(note_type);
    let candidates = lookup_candidates(extraction_context, folder, note_type, &candidate_abstract)
        .await
        .map_err(|e| format!("candidate lookup failed: {e}"))?;

    if candidates.is_empty() {
        return Ok(NoveltyCheckResult {
            assessment: NoveltyAssessment::Novel,
            existing_note_id: None,
        });
    }

    let response = complete(
        extraction_context.provider,
        CompletionRequest {
            system: NOVELTY_SYSTEM_PROMPT.to_string(),
            prompt: build_novelty_prompt(note_type, note, &candidate_abstract, &candidates),
            max_tokens: 300,
        },
    )
    .await
    .map_err(|e| format!("semantic compare failed: {e}"))?;

    let decision: NoveltyDecision = serde_json::from_str(response.text.trim())
        .map_err(|e| format!("invalid novelty decision json: {e}"))?;

    match decision.decision {
        NoveltyDecisionKind::Novel => Ok(NoveltyCheckResult {
            assessment: NoveltyAssessment::Novel,
            existing_note_id: None,
        }),
        NoveltyDecisionKind::AlreadyKnown => {
            let existing_note_id = decision
                .existing_note_id
                .filter(|id| candidates.iter().any(|candidate| candidate.id == *id))
                .ok_or_else(|| {
                    "already_known decision missing valid existing_note_id".to_string()
                })?;
            tracing::debug!(
                session_id = %extraction_context.session_id,
                note_type = %note_type,
                title = %note.title,
                existing_note_id = %existing_note_id,
                "llm_extraction: semantic duplicate decision returned already_known"
            );
            Ok(NoveltyCheckResult {
                assessment: NoveltyAssessment::Duplicate,
                existing_note_id: Some(existing_note_id),
            })
        }
    }
}

fn assess_quality_gate(
    note_type: &str,
    note: &ExtractedNote,
    novelty: &NoveltyCheckResult,
) -> QualityAssessment {
    let specificity = has_specificity(note);
    let generality = has_generality(note);
    let durability = has_durability(note);
    let type_fit = matches_type_semantics(note_type, note);
    let novelty_assessment = novelty.assessment;

    let mut reasons = Vec::new();
    if !specificity {
        reasons.push("insufficient_specificity");
    }
    if !generality {
        reasons.push("task_local_or_overly_narrow");
    }
    if !durability {
        reasons.push("not_durable_beyond_current_task");
    }
    if !type_fit {
        reasons.push("type_fit_mismatch");
    }
    if novelty_assessment == NoveltyAssessment::Duplicate {
        reasons.push("semantic_duplicate_of_existing_note");
    }

    let outcome = if novelty_assessment == NoveltyAssessment::Duplicate {
        ExtractionOutcome::MergeIntoExisting
    } else if !specificity || !type_fit {
        ExtractionOutcome::Discard
    } else if !generality || !durability {
        ExtractionOutcome::DowngradeToWorkingSpec
    } else {
        ExtractionOutcome::DurableWrite
    };

    QualityAssessment {
        specificity,
        generality,
        durability,
        novelty: novelty_assessment,
        type_fit,
        outcome,
        reasons,
    }
}

fn has_specificity(note: &ExtractedNote) -> bool {
    let text = normalized_text(note);
    if text.split_whitespace().count() < 8 {
        return false;
    }
    let signals = [
        text.contains("situation"),
        text.contains("constraint"),
        text.contains("result"),
        text.contains("lesson"),
        text.contains("approach"),
        text.contains("recommended"),
        text.contains("why it works"),
        text.contains("prevention"),
        text.contains("recovery"),
        text.contains('/'),
        text.contains("`"),
        !note.scope_paths.is_empty(),
    ];
    signals.into_iter().filter(|flag| *flag).count() >= 2
}

fn has_generality(note: &ExtractedNote) -> bool {
    let text = normalized_text(note);
    let positive = [
        "reusable", "future", "across", "multiple", "general", "whenever", "teams", "tasks",
        "pattern", "lesson", "prevent",
    ];
    let negative = [
        "this task",
        "current task",
        "temporary",
        "for now",
        "wip",
        "working spec",
        "session-only",
        "local experiment",
    ];
    positive.iter().any(|token| text.contains(token))
        && !negative.iter().any(|token| text.contains(token))
}

fn has_durability(note: &ExtractedNote) -> bool {
    let text = normalized_text(note);
    if text.split_whitespace().count() < MIN_DURABLE_WORDS {
        return false;
    }
    let durable_markers = [
        "guideline",
        "recommend",
        "use when",
        "avoid",
        "prevention",
        "tradeoff",
        "lesson",
        "result",
        "constraint",
    ];
    let transient_markers = [
        "todo",
        "next step",
        "open question",
        "hypothesis",
        "investigate",
        "maybe",
        "might",
        "could",
    ];
    durable_markers.iter().any(|token| text.contains(token))
        && !transient_markers.iter().any(|token| text.contains(token))
}

fn matches_type_semantics(note_type: &str, note: &ExtractedNote) -> bool {
    let text = normalized_text(note);
    match note_type {
        "pattern" => {
            contains_any(
                &text,
                &[
                    "reusable",
                    "recommended",
                    "approach",
                    "use when",
                    "when to use",
                ],
            ) && contains_any(&text, &["because", "why", "tradeoff", "works"])
        }
        "pitfall" => {
            contains_any(
                &text,
                &["pitfall", "failure", "error", "smell", "trigger", "symptom"],
            ) && contains_any(&text, &["prevent", "recovery", "resolve", "avoid"])
        }
        "case" => {
            contains_any(
                &text,
                &[
                    "situation",
                    "constraint",
                    "result",
                    "lesson",
                    "worked",
                    "failed",
                ],
            ) && contains_any(
                &text,
                &["approach", "did", "implemented", "fixed", "resolved"],
            )
        }
        _ => false,
    }
}

fn contains_any(text: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|token| text.contains(token))
}

fn normalized_text(note: &ExtractedNote) -> String {
    format!("{}\n{}", note.title, note.content).to_lowercase()
}

async fn lookup_candidates(
    extraction_context: &ExtractionContext<'_>,
    folder: &str,
    note_type: &str,
    candidate_abstract: &str,
) -> djinn_db::Result<Vec<djinn_db::NoteDedupCandidate>> {
    #[cfg(test)]
    if let Some(lookup) = extraction_context.candidate_lookup.override_lookup {
        return Ok(lookup(
            extraction_context.project_id,
            folder,
            note_type,
            candidate_abstract,
        ));
    }

    extraction_context
        .note_repo
        .dedup_candidates(
            extraction_context.project_id,
            folder,
            note_type,
            candidate_abstract,
            NOVELTY_CANDIDATE_LIMIT,
        )
        .await
}

fn summarize_candidate_note(note: &ExtractedNote) -> String {
    let trimmed = note.content.trim();
    if trimmed.is_empty() {
        note.title.trim().to_string()
    } else {
        format!("{}\n\n{}", note.title.trim(), trimmed)
    }
}

fn build_novelty_prompt(
    note_type: &str,
    note: &ExtractedNote,
    candidate_abstract: &str,
    candidates: &[djinn_db::NoteDedupCandidate],
) -> String {
    let candidate_lines = candidates
        .iter()
        .map(|candidate| {
            let summary = candidate
                .abstract_
                .as_deref()
                .or(candidate.overview.as_deref())
                .unwrap_or("");
            format!(
                "- id: {}\n  title: {}\n  summary: {}",
                candidate.id, candidate.title, summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Note type: {note_type}\nProposed extracted note title: {title}\nProposed extracted note summary:\n{candidate_abstract}\n\nExisting candidates:\n{candidate_lines}\n\nReturn JSON only in this schema:\n{{\"decision\":\"already_known\"|\"novel\",\"existing_note_id\":\"candidate-id-or-null\"}}\nChoose already_known only when the proposed note is semantically the same knowledge as one existing candidate. Otherwise choose novel.",
        title = note.title,
    )
}

// ── JSON parsing helpers ──────────────────────────────────────────────────────

/// Parse the LLM response text into an `ExtractionResponse`.
///
/// The LLM is asked to return pure JSON, but may wrap it in a markdown fence
/// or include leading/trailing whitespace. We strip common wrappers before
/// parsing.
fn parse_extraction_response(text: &str) -> Result<ExtractionResponse, String> {
    let text = text.trim();

    // Strip optional markdown code fences: ```json ... ``` or ``` ... ```
    let text = if let Some(inner) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
    {
        inner.trim_start()
    } else {
        text
    };
    let text = if let Some(inner) = text.strip_suffix("```") {
        inner.trim_end()
    } else {
        text
    };

    serde_json::from_str::<ExtractionResponse>(text).map_err(|e| format!("JSON parse error: {e}"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::actors::slot::session_extraction::ExtractionQuality;
    use crate::test_helpers::{agent_context_from_db, create_test_db, test_path};

    #[test]
    fn parse_extraction_response_valid_json() {
        let json = r#"{"cases":[{"title":"T","content":"C"}],"patterns":[],"pitfalls":[]}"#;
        let result = parse_extraction_response(json).expect("valid json");
        assert_eq!(result.cases.len(), 1);
        assert_eq!(result.cases[0].title, "T");
        assert!(result.patterns.is_empty());
        assert!(result.pitfalls.is_empty());
    }

    #[test]
    fn parse_extraction_response_strips_markdown_fence() {
        let json = "```json\n{\"cases\":[],\"patterns\":[],\"pitfalls\":[]}\n```";
        let result = parse_extraction_response(json).expect("markdown-wrapped json");
        assert!(result.cases.is_empty());
    }

    #[test]
    fn parse_extraction_response_strips_plain_fence() {
        let json = "```\n{\"cases\":[],\"patterns\":[],\"pitfalls\":[]}\n```";
        let result = parse_extraction_response(json).expect("plain-fenced json");
        assert!(result.cases.is_empty());
    }

    #[test]
    fn parse_extraction_response_empty_arrays_when_fields_missing() {
        let json = r#"{}"#;
        let result = parse_extraction_response(json).expect("empty object");
        assert!(result.cases.is_empty());
        assert!(result.patterns.is_empty());
        assert!(result.pitfalls.is_empty());
    }

    #[test]
    fn parse_extraction_response_returns_error_on_invalid_json() {
        let result = parse_extraction_response("not json");
        assert!(result.is_err());
    }

    #[test]
    fn extraction_quality_defaults_to_zero() {
        assert_eq!(ExtractionQuality::default().novelty_skipped, 0);
    }

    #[tokio::test]
    async fn run_llm_extraction_returns_early_when_session_has_no_task_id() {
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let ctx = agent_context_from_db(db.clone(), cancel);

        // Create a session without task_id via SessionRepository
        let session_repo =
            djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let project_repo =
            djinn_db::ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        // Need a project first
        let id = uuid::Uuid::now_v7().to_string();
        let project_path = test_path(&format!("djinn-llm-extraction-no-task-{id}-"));
        let project = project_repo
            .create(
                &format!("proj-{id}"),
                project_path.to_string_lossy().as_ref(),
            )
            .await
            .expect("create project");

        let session = session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &project.id,
                task_id: None, // no task_id
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .expect("create session");

        let taxonomy = SessionTaxonomy::default();

        // Should return early without panicking
        run_llm_extraction(session.id, taxonomy, ctx).await;
    }

    #[tokio::test]
    async fn run_llm_extraction_graceful_degradation_when_provider_unavailable() {
        let db = create_test_db();
        let cancel = CancellationToken::new();
        let ctx = agent_context_from_db(db.clone(), cancel);

        let events = djinn_core::events::EventBus::noop();
        let session_repo = djinn_db::SessionRepository::new(db.clone(), events.clone());
        let project_repo = djinn_db::ProjectRepository::new(db.clone(), events.clone());
        let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
        let epic_repo = djinn_db::EpicRepository::new(db.clone(), events.clone());

        let id = uuid::Uuid::now_v7().to_string();
        let project_path = test_path(&format!("djinn-llm-extraction-provider-{id}-"));
        let project = project_repo
            .create(
                &format!("proj-{id}"),
                project_path.to_string_lossy().as_ref(),
            )
            .await
            .expect("create project");

        let epic = epic_repo
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "test-epic",
                    description: "desc",
                    emoji: "🧪",
                    color: "blue",
                    owner: "test",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: None,
                    originating_adr_id: None,
                },
            )
            .await
            .expect("create epic");

        let task = task_repo
            .create_in_project(
                &project.id,
                Some(&epic.id),
                "test-task",
                "test task description",
                "test design",
                "task",
                2,
                "test",
                None,
                None,
            )
            .await
            .expect("create task");

        let session = session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test-model",
                agent_type: "worker",
                worktree_path: None,
                metadata_json: None,
            })
            .await
            .expect("create session");

        let taxonomy = SessionTaxonomy {
            files_changed: 5,
            errors: 3,
            tools_used: 8,
            notes_read: 1,
            notes_written: 2,
            tasks_transitioned: 1,
            changed_file_paths: vec![],
            extraction_quality: ExtractionQuality::default(),
        };

        // No credentials configured → resolve_memory_provider will fail → graceful skip
        // Should not panic
        run_llm_extraction(session.id, taxonomy, ctx).await;
    }
}
