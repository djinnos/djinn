// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
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
//!
//! # Wiring (Phase 2.2)
//!
//! [`run_llm_extraction`] is driven by
//! `session_extraction::run_post_session_extraction`, which runs server-side
//! (fire-and-forget) when a task-run completes. The production path resolves
//! the model via creator-scoped dispatch-style resolution, with an explicit
//! org-shared/no-user memory-provider fallback when the creator-scoped path
//! cannot resolve.
//! The file-level `#[allow(dead_code)]` is retained only to cover the
//! `_with_provider` test entry points and helpers exercised solely by tests.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{
    CreateConsolidationRunMetric, NoteConsolidationRepository, NoteRepository,
    NoteRevisionCreateState, NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation,
    NoteRevisionReason, NoteRevisionSubsystem, ProjectRepository, SessionRepository,
    TaskRepository, TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance,
    assess_note_quality, folder_for_type, permalink_for,
};
use djinn_provider::provider::{LlmProvider, TelemetryMeta, create_provider};
use djinn_provider::{CompletionRequest, complete, resolve_memory_provider_for_user};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::session_extraction::SessionTaxonomy;
use crate::host::{KnowledgeBranchTarget, SlotContext};

const SYSTEM_PROMPT: &str = "You are a knowledge extractor. Given a completed agent session \
summary, extract reusable knowledge as structured notes. Respond with valid JSON only.";

/// Maximum novelty candidates to check before creating a new note.
const NOVELTY_CANDIDATE_LIMIT: usize = 3;

/// Maximum Unicode scalar values from one existing candidate body included in
/// the novelty prompt. The repository still carries the complete body; this cap
/// keeps the LLM context bounded deterministically.
const NOVELTY_CANDIDATE_CONTENT_CHAR_CAP: usize = 4_000;

/// Confidence signal applied to an existing note when a new extraction is
/// semantically judged to be already known.
const DUPLICATE_CONFIDENCE_SIGNAL: f64 = 0.65;

/// Extraction may propose only a small confidence adjustment. The eventual
/// repository mutation path applies its own policy; this bound prevents a
/// model response from proposing an implausibly large change in the first
/// place.
const MAX_REVISION_CONFIDENCE_DELTA: f64 = 0.25;

const EXTRACTION_SYSTEM_PROMPT: &str = SYSTEM_PROMPT;
const NOVELTY_SYSTEM_PROMPT: &str = "You are a semantic novelty judge for extracted knowledge notes. Compare a proposed extracted note against existing candidate notes using their bounded full bodies. Respond with valid JSON only.";
/// The evidence merge has its own strict response contract so malformed model
/// output can take the existing confidence-only fallback without aborting work.
const EVIDENCE_MERGE_SYSTEM_PROMPT: &str = "You merge attributed session evidence into an existing knowledge note. Preserve specific evidence; do not replace the note wholesale. Respond with valid JSON only.";
/// Curated/high-confidence notes are never rewritten by background extraction.
const EVIDENCE_MERGE_MAX_CONFIDENCE: f64 = 0.8;

/// Max characters of session transcript fed to the extraction LLM.
const TRANSCRIPT_EXCERPT_CHARS: usize = 12_000;

/// Max output tokens for the extraction completion.
///
/// The extraction returns up to three structured note types in one JSON object
/// (up to 3 cases + 3 patterns + 2 pitfalls), and each durable note must carry
/// its full set of required ADR-054 markdown sections. The previous 1024-token
/// cap routinely truncated the JSON mid-array, which then failed to parse and
/// silently dropped every note. 4096 gives enough headroom for the full
/// structured payload while staying well within the model's context window.
const EXTRACTION_MAX_TOKENS: u32 = 4096;

/// Hard outer bound on the post-session extraction LLM completion.
///
/// Post-session knowledge extraction is best-effort background work that runs
/// on the slot's finalize path. Without an explicit bound it inherits the
/// provider's own request timeout (up to ~10 minutes for a streaming
/// completion), which needlessly pins a finalize task and slot-pool resources
/// on a stalled memory provider — indirect pressure on the same
/// session-exit→teardown→redispatch window implicated in the 2026-07-09
/// whole-board freeze. Cap it explicitly and reuse the existing
/// "LLM completion failed; skipping extraction" degrade path on elapse; the
/// value sits above the provider's inner per-attempt timeout (plus its one
/// transient retry) so it only fires when that inner bound is itself defeated.
const EXTRACTION_LLM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

const NO_LLM_PROVIDER_WARNING: &str =
    "llm_extraction: no LLM provider available; skipping extraction";
const EXTRACTION_SKIPPED_REASON: &str = "extraction completed without durable note output";

/// Truthful terminal state supplied to post-session knowledge extraction.
///
/// This is deliberately separate from session history: callers without a
/// terminal report must use [`TerminalExtractionOutcome::UnknownHistorical`]
/// instead of deriving a verdict from incomplete evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalExtractionContext {
    pub outcome: TerminalExtractionOutcome,
    /// Present only when terminal task-run data explicitly recorded a review
    /// verdict. `None` means that no review decision may be inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<TerminalReviewDecision>,
}

impl TerminalExtractionContext {
    /// Deliberate context-free compatibility policy for historical callers.
    pub const fn unknown_historical() -> Self {
        Self {
            outcome: TerminalExtractionOutcome::UnknownHistorical,
            review_decision: None,
        }
    }
}

impl Default for TerminalExtractionContext {
    fn default() -> Self {
        Self::unknown_historical()
    }
}

/// Final task-run outcome relevant to interpreting extracted knowledge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalExtractionOutcome {
    Completed,
    Parked {
        classification: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Failed {
        classification: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    UnknownHistorical,
}

/// An explicitly recorded review decision, never an inferred one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum TerminalReviewDecision {
    Approved,
    Rejected {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

enum LlmExtractionProviderResolution {
    Provider(Box<dyn LlmProvider>),
    NoProvider {
        warning_message: &'static str,
        error: String,
    },
}

async fn resolve_llm_extraction_provider_after_creator_attempt(
    db: &djinn_db::Database,
    session_id: &str,
    creator_resolved_provider: Option<Box<dyn LlmProvider>>,
    telemetry: TelemetryMeta,
) -> LlmExtractionProviderResolution {
    if let Some(provider) = creator_resolved_provider {
        return LlmExtractionProviderResolution::Provider(provider);
    }
    tracing::debug!(
        session_id = %session_id,
        provider_resolution_stage = "org_shared_memory_provider_fallback",
        "llm_extraction: creator-scoped provider unavailable; trying org-shared memory-provider fallback"
    );
    match resolve_memory_provider_for_user(db, None).await {
        Ok(provider) => match provider.config_snapshot() {
            Some(mut config) => {
                tracing::debug!(
                    session_id = %session_id,
                    provider_resolution_stage = "org_shared_memory_provider_fallback",
                    provider = %provider.name(),
                    model = %config.model_id,
                    "llm_extraction: org-shared memory-provider fallback resolved provider"
                );
                config.telemetry = Some(telemetry);
                LlmExtractionProviderResolution::Provider(create_provider(config))
            }
            None => {
                tracing::debug!(
                    session_id = %session_id,
                    provider_resolution_stage = "org_shared_memory_provider_fallback",
                    provider = %provider.name(),
                    "llm_extraction: org-shared memory-provider fallback resolved provider"
                );
                LlmExtractionProviderResolution::Provider(provider)
            }
        },
        Err(e) => {
            let error = e.to_string();
            tracing::warn!(
                session_id = %session_id,
                provider_resolution_stage = "org_shared_memory_provider_fallback",
                error = %error,
                "llm_extraction: no LLM provider available; skipping extraction"
            );
            LlmExtractionProviderResolution::NoProvider {
                warning_message: NO_LLM_PROVIDER_WARNING,
                error,
            }
        }
    }
}

async fn resolve_creator_scoped_llm_extraction_provider(
    app_state: &SlotContext,
    session_id: &str,
    task_id: &str,
    creator: Option<String>,
    memory_model_id: &str,
) -> LlmExtractionProviderResolution {
    let attributed_user_id = creator
        .clone()
        .or_else(djinn_core::auth_context::current_user_id);
    let telemetry = crate::helpers::build_telemetry_meta_with_attribution(
        "memory_extraction",
        task_id,
        Some("memory_extraction"),
        attributed_user_id.as_deref(),
    );
    // One-shot completion over a small (taxonomy) prompt — no compaction —
    // so a generous fixed context window is safe.
    const MEMORY_CONTEXT_WINDOW: u32 = 128_000;
    let creator_scoped = djinn_core::auth_context::SESSION_USER_ID
        .scope(
            creator.clone(),
            crate::lifecycle::model_resolution::resolve_model_and_credential(
                memory_model_id,
                task_id,
                app_state,
            ),
        )
        .await;
    let via_creator = match creator_scoped {
        Ok(resolved) => {
            let catalog_provider_id = resolved.catalog_provider_id.clone();
            let model_name = resolved.model_name.clone();
            let base_url = if crate::helpers::resolved_needs_base_url(&resolved) {
                crate::helpers::default_base_url(&catalog_provider_id)
            } else {
                String::new()
            };
            let provider = crate::helpers::build_provider_from_resolved(
                resolved,
                MEMORY_CONTEXT_WINDOW,
                Some(telemetry.clone()),
                None,
                base_url,
            );
            match provider.as_ref() {
                Some(provider) => tracing::debug!(
                    session_id = %session_id,
                    task_id = %task_id,
                    creator_user_id = ?creator,
                    provider_resolution_stage = "creator_scoped_model_credential",
                    catalog_provider_id = %catalog_provider_id,
                    model_id = %memory_model_id,
                    resolved_model = %model_name,
                    provider = %provider.name(),
                    "llm_extraction: creator-scoped model and credential resolved provider"
                ),
                None => tracing::warn!(
                    session_id = %session_id,
                    task_id = %task_id,
                    creator_user_id = ?creator,
                    provider_resolution_stage = "creator_scoped_model_credential",
                    catalog_provider_id = %catalog_provider_id,
                    model_id = %memory_model_id,
                    resolved_model = %model_name,
                    "llm_extraction: creator-scoped model and credential resolved but provider construction failed; trying scoped memory provider fallback"
                ),
            }
            provider
        }
        Err(e) => {
            tracing::debug!(
                session_id = %session_id,
                task_id = %task_id,
                creator_user_id = ?creator,
                provider_resolution_stage = "creator_scoped_model_credential",
                model_id = %memory_model_id,
                error = %e.reason,
                "llm_extraction: creator-scoped model resolution failed; trying scoped memory provider fallback"
            );
            None
        }
    };
    resolve_llm_extraction_provider_after_creator_attempt(
        &app_state.db,
        session_id,
        via_creator,
        telemetry,
    )
    .await
}

fn render_terminal_context(context: &TerminalExtractionContext) -> String {
    let outcome = match &context.outcome {
        TerminalExtractionOutcome::Completed => "completed successfully".to_string(),
        TerminalExtractionOutcome::Parked {
            classification,
            reason,
        } => format!(
            "parked (classification: {classification}; reason: {})",
            reason.as_deref().unwrap_or("not recorded")
        ),
        TerminalExtractionOutcome::Failed {
            classification,
            reason,
        } => format!(
            "failed (classification: {classification}; reason: {})",
            reason.as_deref().unwrap_or("not recorded")
        ),
        TerminalExtractionOutcome::UnknownHistorical => {
            "unknown/historical; no terminal verdict was supplied".to_string()
        }
    };
    let review = match &context.review_decision {
        Some(TerminalReviewDecision::Approved) => "approved".to_string(),
        Some(TerminalReviewDecision::Rejected { reason }) => format!(
            "rejected (reason: {})",
            reason.as_deref().unwrap_or("not recorded")
        ),
        None => "no explicit review decision recorded; do not infer one".to_string(),
    };
    let guidance = match (&context.outcome, &context.review_decision) {
        (_, Some(TerminalReviewDecision::Rejected { .. }))
        | (
            TerminalExtractionOutcome::Parked { .. } | TerminalExtractionOutcome::Failed { .. },
            _,
        ) => {
            "This rejected, parked, or failed work is evidence of a failed approach or pitfall. Extract pitfalls or failed cases when supported; do not frame it as a neutral or successful pattern."
        }
        (TerminalExtractionOutcome::Completed, _) => {
            "Completed work may support successful cases or patterns, but only when the transcript provides clear evidence."
        }
        (TerminalExtractionOutcome::UnknownHistorical, _) => {
            "Terminal outcome is unknown. Do not assume success, failure, or a review verdict; rely only on explicit session evidence."
        }
    };
    format!(
        "Outcome: {outcome}\nExplicit review decision: {review}\nExtraction guidance: {guidance}"
    )
}

/// Render the extraction prompt the LLM sees.
///
/// Exposed (not inlined into `run_llm_extraction_inner`) so the prompt schema
/// can be unit-tested independently of the rest of the extraction pipeline.
/// The prompt asks for one `applies_when` anchor per case/pattern/pitfall
/// note, distinct from the durable markdown body, and lists the exact
/// ADR-054 markdown headings each note type must include. The headings must
/// remain present so a future prompt edit does not silently regress the
/// durable note schema (T2 of the x72l epic).
fn build_extraction_prompt(
    title: &str,
    description: &str,
    taxonomy_json: &str,
    transcript: &str,
    scope_json: &str,
    terminal_context: &TerminalExtractionContext,
) -> String {
    let terminal_context = render_terminal_context(terminal_context);
    format!(
        "Task: {title}\n\
         Description: {description}\n\n\
         TERMINAL TASK-RUN CONTEXT (authoritative; do not infer absent verdicts):\n{terminal_context}\n\
         Session event counts: {taxonomy_json}\n\n\
         Session transcript (excerpt — assistant reasoning, tool actions, and results; \
         this is the actual work to distill knowledge from):\n{transcript}\n\n\
         Files touched were in these areas: {scope_json}\n\
         Include a \"scope_paths\" array per note with relevant path prefixes from the list above.\n\n\
         Extract knowledge from this session. Return JSON:\n\
         {{\n\
           \"cases\": [{{\"title\": \"...\", \"content\": \"Markdown note using the exact required case headings\", \"applies_when\": \"One sentence describing when this case applies.\", \"scope_paths\": [\"...\"]}}],\n\
           \"patterns\": [{{\"title\": \"...\", \"content\": \"Markdown note using the exact required pattern headings\", \"applies_when\": \"One sentence describing when this pattern applies.\", \"scope_paths\": [\"...\"]}}],\n\
           \"pitfalls\": [{{\"title\": \"...\", \"content\": \"Markdown note using the exact required pitfall headings\", \"applies_when\": \"One sentence describing when this pitfall applies.\", \"scope_paths\": [\"...\"]}}],\n\
           \"revision_operations\": [{{\"kind\":\"patch\",\"target_note_id\":\"UUID\",\"before_text\":\"exact current note content\",\"after_text\":\"replacement content\",\"confidence_delta\":0.1,\"reason\":\"why this correction is supported\"}},{{\"kind\":\"deprecate_with_supersedes\",\"deprecated_note_id\":\"UUID\",\"superseding_note_id\":\"UUID\",\"reason\":\"why the replacement supersedes it\"}}]\n\
         }}\n\
         Required durable templates:\n\
         Pattern content must contain exactly these markdown headings in order:\n\
         ## Context\n## Problem shape\n## Recommended approach\n## Why it works\n## Tradeoffs / limits\n## When to use\n## When not to use\n## Related\n\
         Pitfall content must contain exactly these markdown headings in order:\n\
         ## Trigger / smell\n## Failure mode\n## Observable symptoms\n## Prevention\n## Recovery\n## Related\n\
         Case content must contain exactly these markdown headings in order:\n\
         ## Situation\n## Constraint\n## Approach taken\n## Result\n## Why it worked / failed\n## Reusable lesson\n## Related\n\
         For every note, also include an \"applies_when\" field: a single concise sentence \
         describing the situation where the note is the right thing to recall. \
         \"applies_when\" must be DISTINCT from the markdown body (do not duplicate a heading) \
         and must be one sentence ending in a period. If you cannot articulate a useful \
         applies_when for a note, omit that note instead of returning a vague one. \
         If you cannot fill every required section for a note type, omit that note instead of returning a shorter paragraph.\n\
         \"revision_operations\" is optional and may be omitted or be []. When present, every item MUST use exactly one of the tagged \"kind\" shapes above; do not emit untagged or ad-hoc operation objects. IDs are proposals only: the server validates ID format, project, eligibility, current content, and policy before any mutation. For patch operations, before_text and after_text must be non-blank, reason must be non-blank, and confidence_delta must be between -0.25 and 0.25 inclusive. For deprecate_with_supersedes, both IDs and reason must be non-blank and the IDs must differ.\n\
         Return empty arrays if nothing significant was learned. \
         Maximum 3 cases, 3 patterns, 2 pitfalls.\n\
         Only extract if there is clear signal (high errors+files_changed suggests pitfalls; \
         many notes_written suggests patterns).",
        title = title,
        description = description,
        taxonomy_json = taxonomy_json,
        scope_json = scope_json,
        terminal_context = terminal_context,
    )
}

/// Render a compact transcript excerpt for the extraction prompt: assistant
/// reasoning, tool actions, and (truncated) tool results, capped to `max_chars`
/// and tail-biased so the session's outcome/conclusions are retained. The
/// taxonomy only carries event COUNTS; without the actual content here the LLM
/// has nothing to distill into case/pattern/pitfall notes (and returns empty
/// arrays every time).
fn build_transcript_excerpt(messages: &[djinn_core::message::Message], max_chars: usize) -> String {
    use djinn_core::message::{ContentBlock, Role};
    fn take_chars(s: &str, n: usize) -> String {
        if s.chars().count() > n {
            let mut t: String = s.chars().take(n).collect();
            t.push('…');
            t
        } else {
            s.to_string()
        }
    }
    fn blocks_text(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.trim()),
                _ => None,
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
    let mut lines: Vec<String> = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::System => continue,
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    let t = text.trim();
                    if !t.is_empty() {
                        lines.push(format!("{role}: {t}"));
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    lines.push(format!(
                        "{role} → {name}({})",
                        take_chars(&input.to_string(), 200)
                    ));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let body = take_chars(&blocks_text(content), 600);
                    if !body.is_empty() {
                        let tag = if *is_error {
                            "tool error"
                        } else {
                            "tool result"
                        };
                        lines.push(format!("{tag}: {body}"));
                    }
                }
                _ => {}
            }
        }
    }
    let full = lines.join("\n");
    if full.len() <= max_chars {
        return full;
    }
    // Tail-biased: keep the most recent content, aligned to a char boundary.
    let target = full.len() - max_chars;
    let start = full
        .char_indices()
        .find(|(i, _)| *i >= target)
        .map(|(i, _)| i)
        .unwrap_or(full.len());
    format!("…(earlier turns omitted)…\n{}", &full[start..])
}

const MIN_DURABLE_WORDS: usize = 16;

#[derive(Debug, Deserialize, Default, Clone)]
struct ExtractedNote {
    title: String,
    content: String,
    /// One-sentence retrieval situation where the note applies. Distinct from
    /// the durable markdown body. Prompted and parsed as `applies_when` (the
    /// human-facing prompt term) with `retrieval_anchor` accepted as an alias
    /// for callers that already use the storage-field name. Missing or empty
    /// values are tolerated so a model that forgets the field does not break
    /// extraction — durable write happens, just without an anchor.
    #[serde(default, alias = "applies_when")]
    retrieval_anchor: Option<String>,
    #[serde(default)]
    scope_paths: Vec<String>,
}

impl ExtractedNote {
    /// Returns the retrieval anchor as a normalized one-sentence string, or
    /// `None` when the model did not provide one. Normalization trims
    /// surrounding whitespace and treats empty / whitespace-only values as
    /// missing so the persistence path never stores a blank anchor.
    fn normalized_anchor(&self) -> Option<String> {
        self.retrieval_anchor
            .as_deref()
            .map(str::trim)
            .filter(|anchor| !anchor.is_empty())
            .map(str::to_owned)
    }
}

/// Normalized dedup key for an extracted note: lowercase+trimmed title paired
/// with its note_type. Two notes with the same key carry the same knowledge as
/// far as this batch is concerned.
fn note_dedup_key(note_type: &str, note: &ExtractedNote) -> (String, String) {
    (
        note.title.trim().to_lowercase(),
        note_type.trim().to_lowercase(),
    )
}

/// Collapse notes that are duplicated WITHIN a single extraction by their
/// normalized (title, note_type) key, preserving first-seen order. Returns the
/// deduplicated `(note_type, note)` list and the number of duplicates dropped.
fn dedup_extracted_notes(
    extracted: &ExtractionResponse,
) -> (Vec<(&'static str, ExtractedNote)>, usize) {
    let candidates: [(&'static str, &[ExtractedNote]); 3] = [
        ("case", extracted.cases.as_slice()),
        ("pattern", extracted.patterns.as_slice()),
        ("pitfall", extracted.pitfalls.as_slice()),
    ];
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut out: Vec<(&'static str, ExtractedNote)> = Vec::new();
    let mut dupes = 0usize;
    for (note_type, notes) in candidates {
        for note in notes {
            if seen.insert(note_dedup_key(note_type, note)) {
                out.push((note_type, note.clone()));
            } else {
                dupes += 1;
            }
        }
    }
    (out, dupes)
}

impl ExtractionContext<'_> {
    fn revision_provenance(&self) -> djinn_db::Result<TrustedNoteRevisionProvenance> {
        TrustedNoteRevisionProvenance::new(
            Some(self.session_id.to_owned()),
            Some(self.task_id.to_owned()),
            self.task_run_id.map(ToOwned::to_owned),
        )
        .map_err(|e| djinn_db::Error::InvalidData(e.to_string()))
    }
    async fn mutate_existing(
        &self,
        existing: &djinn_memory::Note,
        content: String,
        confidence: f64,
        event_kind: NoteRevisionEventKind,
        reason: &str,
    ) -> djinn_db::Result<djinn_db::NoteRevisionMutationResult> {
        self.note_repo
            .mutate_with_revision(NoteRevisionMutation {
                project_id: self.project_id.to_owned(),
                note_id: Some(existing.id.clone()),
                event_kind,
                desired: NoteRevisionDesiredState::Existing {
                    content,
                    confidence,
                },
                attribution: TrustedNoteRevisionAttribution::system(
                    NoteRevisionSubsystem::Extraction,
                ),
                provenance: self.revision_provenance()?,
                reason: NoteRevisionReason::new(reason)
                    .map_err(|e| djinn_db::Error::InvalidData(e.to_string()))?,
            })
            .await
    }

    async fn create_extracted_note(
        &self,
        title: &str,
        content: &str,
        note_type: &str,
        scope_paths_json: &str,
        retrieval_anchor: Option<&str>,
    ) -> djinn_db::Result<djinn_db::NoteRevisionMutationResult> {
        self.note_repo
            .mutate_with_revision(NoteRevisionMutation {
                project_id: self.project_id.to_owned(),
                note_id: Some(uuid::Uuid::now_v7().to_string()),
                event_kind: NoteRevisionEventKind::Created,
                desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                    title: title.to_owned(),
                    permalink: permalink_for(note_type, title),
                    content: content.to_owned(),
                    note_type: note_type.to_owned(),
                    folder: folder_for_type(note_type).to_owned(),
                    status: "active".to_owned(),
                    tags: "[]".to_owned(),
                    retrieval_anchor: retrieval_anchor.map(ToOwned::to_owned),
                    scope_paths: scope_paths_json.to_owned(),
                    confidence: 0.5,
                }),
                attribution: TrustedNoteRevisionAttribution::system(
                    NoteRevisionSubsystem::Extraction,
                ),
                provenance: self.revision_provenance()?,
                reason: NoteRevisionReason::new("created note from completed session extraction")
                    .map_err(|e| djinn_db::Error::InvalidData(e.to_string()))?,
            })
            .await
    }
}

#[derive(Debug, Deserialize, Default)]
struct ExtractionResponse {
    #[serde(default)]
    cases: Vec<ExtractedNote>,
    #[serde(default)]
    patterns: Vec<ExtractedNote>,
    #[serde(default)]
    pitfalls: Vec<ExtractedNote>,
    #[serde(default)]
    revision_operations: Vec<RevisionOperation>,
}
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RevisionOperation {
    Patch {
        target_note_id: String,
        before_text: String,
        after_text: String,
        confidence_delta: f64,
        reason: String,
    },
    DeprecateWithSupersedes {
        deprecated_note_id: String,
        superseding_note_id: String,
        reason: String,
    },
}

/// Stable refusal vocabulary for syntactically invalid extraction revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevisionOperationRefusalReason {
    MalformedOperationShape,
    BlankReason,
    BlankRequiredText,
    InvalidNoteId,
    SelfReplacement,
    ConfidenceDeltaOutOfRange,
}

impl std::fmt::Display for RevisionOperationRefusalReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        let reason = match self {
            Self::MalformedOperationShape => "malformed_operation_shape",
            Self::BlankReason => "blank_reason",
            Self::BlankRequiredText => "blank_required_text",
            Self::InvalidNoteId => "invalid_note_id",
            Self::SelfReplacement => "self_replacement",
            Self::ConfidenceDeltaOutOfRange => "confidence_delta_out_of_range",
        };
        formatter.write_str(reason)
    }
}

fn validate_revision_operations(
    operations: &mut [RevisionOperation],
) -> Result<(), RevisionOperationRefusalReason> {
    for operation in operations {
        match operation {
            RevisionOperation::Patch {
                target_note_id,
                before_text,
                after_text,
                confidence_delta,
                reason,
            } => {
                validate_note_id(target_note_id)?;
                if before_text.trim().is_empty() || after_text.trim().is_empty() {
                    return Err(RevisionOperationRefusalReason::BlankRequiredText);
                }
                if !confidence_delta.is_finite()
                    || confidence_delta.abs() > MAX_REVISION_CONFIDENCE_DELTA
                {
                    return Err(RevisionOperationRefusalReason::ConfidenceDeltaOutOfRange);
                }
                normalize_reason(reason)?;
            }
            RevisionOperation::DeprecateWithSupersedes {
                deprecated_note_id,
                superseding_note_id,
                reason,
            } => {
                validate_note_id(deprecated_note_id)?;
                validate_note_id(superseding_note_id)?;
                if deprecated_note_id == superseding_note_id {
                    return Err(RevisionOperationRefusalReason::SelfReplacement);
                }
                normalize_reason(reason)?;
            }
        }
    }
    Ok(())
}

fn validate_note_id(note_id: &str) -> Result<(), RevisionOperationRefusalReason> {
    match uuid::Uuid::parse_str(note_id) {
        Ok(id) if !id.is_nil() => Ok(()),
        _ => Err(RevisionOperationRefusalReason::InvalidNoteId),
    }
}

fn normalize_reason(reason: &mut String) -> Result<(), RevisionOperationRefusalReason> {
    let normalized = reason.trim();
    if normalized.is_empty() {
        return Err(RevisionOperationRefusalReason::BlankReason);
    }
    if normalized.len() != reason.len() {
        *reason = normalized.to_owned();
    }
    Ok(())
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
    required_structure: bool,
    outcome: ExtractionOutcome,
    reasons: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct NoveltyCheckResult {
    assessment: NoveltyAssessment,
    existing_note_id: Option<String>,
    selected_candidate: Option<djinn_db::NoteDedupCandidate>,
}

#[derive(Debug, Deserialize)]
struct EvidenceMergeResponse {
    content: String,
}

#[cfg(any(test, feature = "test-support"))]
pub type CandidateLookupOverride = fn(&str, &str, &str, &str) -> Vec<djinn_db::NoteDedupCandidate>;

#[allow(clippy::too_many_arguments)]
#[async_trait::async_trait]
pub(crate) trait ExtractionNoteRepository: Send + Sync {
    async fn mutate_with_revision(
        &self,
        mutation: NoteRevisionMutation,
    ) -> djinn_db::Result<djinn_db::NoteRevisionMutationResult>;
    async fn get(&self, id: &str) -> djinn_db::Result<Option<djinn_memory::Note>>;
    async fn update(
        &self,
        id: &str,
        title: &str,
        content: &str,
        tags: &str,
    ) -> djinn_db::Result<djinn_memory::Note>;
    async fn update_confidence(&self, note_id: &str, signal: f64) -> djinn_db::Result<f64>;
    async fn set_confidence(&self, note_id: &str, value: f64) -> djinn_db::Result<()>;
    async fn get_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
    ) -> djinn_db::Result<Option<djinn_memory::Note>>;
    async fn create_db_note_with_scope_and_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
        scope_paths: &str,
        retrieval_anchor: Option<&str>,
    ) -> djinn_db::Result<djinn_memory::Note>;
    async fn create_with_scope_and_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        permalink: Option<&str>,
        tags: &str,
        scope_paths: &str,
        retrieval_anchor: Option<&str>,
    ) -> djinn_db::Result<djinn_memory::Note>;
    async fn create_with_scope(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        permalink: Option<&str>,
        tags: &str,
        scope_paths: &str,
    ) -> djinn_db::Result<djinn_memory::Note>;
    async fn update_scope_paths(
        &self,
        id: &str,
        scope_paths: &str,
    ) -> djinn_db::Result<djinn_memory::Note>;
    async fn dedup_candidates(
        &self,
        project_id: &str,
        folder: &str,
        note_type: &str,
        text: &str,
        limit: usize,
    ) -> djinn_db::Result<Vec<djinn_db::NoteDedupCandidate>>;
}

/// Record the sole terminal event for a run that committed no canonical note
/// revision. Invalid loaded provenance must not be replaced with anonymous
/// attribution.
async fn record_extraction_skipped(
    note_repo: &dyn ExtractionNoteRepository,
    project_id: &str,
    session_id: &str,
    task_id: &str,
    task_run_id: Option<&str>,
) {
    let provenance = match TrustedNoteRevisionProvenance::new(
        Some(session_id.to_owned()),
        Some(task_id.to_owned()),
        task_run_id.map(ToOwned::to_owned),
    ) {
        Ok(provenance) => provenance,
        Err(error) => {
            tracing::error!(%session_id, %task_id, %error, "llm_extraction: invalid trusted extraction provenance");
            return;
        }
    };
    let reason = match NoteRevisionReason::new(EXTRACTION_SKIPPED_REASON) {
        Ok(reason) => reason,
        Err(error) => {
            tracing::error!(%session_id, %error, "llm_extraction: invalid extraction-skipped reason");
            return;
        }
    };
    if let Err(error) = note_repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.to_owned(),
            note_id: None,
            event_kind: NoteRevisionEventKind::ExtractionSkipped,
            desired: NoteRevisionDesiredState::ExtractionSkipped,
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
            provenance,
            reason,
        })
        .await
    {
        tracing::warn!(%session_id, %error, "llm_extraction: failed to record no-output extraction revision");
    }
}

/// Finalize an extraction run from its committed output count. This deliberately
/// accepts only the count returned by canonical mutations, never extraction
/// quality bookkeeping, so every terminal no-output path shares one predicate.
async fn finalize_extraction_output(
    note_repo: &dyn ExtractionNoteRepository,
    project_id: &str,
    session_id: &str,
    task_id: &str,
    task_run_id: Option<&str>,
    durable_output_count: usize,
) {
    if durable_output_count == 0 {
        record_extraction_skipped(note_repo, project_id, session_id, task_id, task_run_id).await;
    }
}

#[async_trait::async_trait]
impl ExtractionNoteRepository for NoteRepository {
    async fn mutate_with_revision(
        &self,
        mutation: NoteRevisionMutation,
    ) -> djinn_db::Result<djinn_db::NoteRevisionMutationResult> {
        NoteRepository::mutate_with_revision(self, mutation).await
    }
    async fn get(&self, id: &str) -> djinn_db::Result<Option<djinn_memory::Note>> {
        NoteRepository::get(self, id).await
    }
    async fn update(
        &self,
        id: &str,
        title: &str,
        content: &str,
        tags: &str,
    ) -> djinn_db::Result<djinn_memory::Note> {
        NoteRepository::update(self, id, title, content, tags).await
    }
    async fn update_confidence(&self, note_id: &str, signal: f64) -> djinn_db::Result<f64> {
        NoteRepository::update_confidence(self, note_id, signal).await
    }
    async fn set_confidence(&self, note_id: &str, value: f64) -> djinn_db::Result<()> {
        NoteRepository::set_confidence(self, note_id, value).await
    }
    async fn get_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
    ) -> djinn_db::Result<Option<djinn_memory::Note>> {
        NoteRepository::get_by_permalink(self, project_id, permalink).await
    }
    async fn create_db_note_with_scope_and_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
        scope_paths: &str,
        retrieval_anchor: Option<&str>,
    ) -> djinn_db::Result<djinn_memory::Note> {
        NoteRepository::create_db_note_with_scope_and_retrieval_anchor(
            self,
            project_id,
            title,
            content,
            note_type,
            tags,
            scope_paths,
            retrieval_anchor,
        )
        .await
    }
    async fn create_with_scope_and_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        permalink: Option<&str>,
        tags: &str,
        scope_paths: &str,
        retrieval_anchor: Option<&str>,
    ) -> djinn_db::Result<djinn_memory::Note> {
        NoteRepository::create_with_scope_and_retrieval_anchor(
            self,
            project_id,
            title,
            content,
            note_type,
            permalink,
            tags,
            scope_paths,
            retrieval_anchor,
        )
        .await
    }
    async fn create_with_scope(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        permalink: Option<&str>,
        tags: &str,
        scope_paths: &str,
    ) -> djinn_db::Result<djinn_memory::Note> {
        NoteRepository::create_with_scope(
            self,
            project_id,
            title,
            content,
            note_type,
            permalink,
            tags,
            scope_paths,
        )
        .await
    }
    async fn update_scope_paths(
        &self,
        id: &str,
        scope_paths: &str,
    ) -> djinn_db::Result<djinn_memory::Note> {
        NoteRepository::update_scope_paths(self, id, scope_paths).await
    }
    async fn dedup_candidates(
        &self,
        project_id: &str,
        folder: &str,
        note_type: &str,
        text: &str,
        limit: usize,
    ) -> djinn_db::Result<Vec<djinn_db::NoteDedupCandidate>> {
        NoteRepository::dedup_candidates(self, project_id, folder, note_type, text, limit).await
    }
}

struct ExtractionContext<'a> {
    note_repo: &'a dyn ExtractionNoteRepository,
    provider: &'a dyn LlmProvider,
    project_id: &'a str,
    project_path: &'a str,
    knowledge_branch_target: &'a KnowledgeBranchTarget,
    session_id: &'a str,
    task_id: &'a str,
    task_run_id: Option<&'a str>,
    task_short_id: &'a str,
    task_title: &'a str,
    task_description: &'a str,
    provenance: &'a str,
    caller_attributed: bool,
    session_scope_paths: &'a [String],
    #[cfg(any(test, feature = "test-support"))]
    candidate_lookup: CandidateLookup,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy)]
struct CandidateLookup {
    override_lookup: Option<CandidateLookupOverride>,
}

#[cfg(any(test, feature = "test-support"))]
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

/// Run LLM-based knowledge extraction for a completed session.
///
/// Context-free compatibility entry point. It deliberately supplies an
/// unknown/historical outcome and no review decision rather than fabricating
/// a terminal verdict from session history.
pub async fn run_llm_extraction(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: SlotContext,
) {
    run_llm_extraction_inner(
        session_id,
        taxonomy,
        app_state,
        TerminalExtractionContext::unknown_historical(),
        None,
        #[cfg(any(test, feature = "test-support"))]
        None,
    )
    .await;
}

/// Run LLM extraction with terminal task-run context supplied by the live completion path.
///
/// Unlike [`run_llm_extraction`], this preserves the reported outcome and any
/// explicit review decision in the extraction prompt.
pub async fn run_llm_extraction_with_terminal_context(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: SlotContext,
    terminal_context: TerminalExtractionContext,
) {
    run_llm_extraction_inner(
        session_id,
        taxonomy,
        app_state,
        terminal_context,
        None,
        #[cfg(any(test, feature = "test-support"))]
        None,
    )
    .await;
}

/// Test-only contextual entry point with an injected provider.
///
/// This keeps production provider resolution intact while allowing the
/// post-session orchestration test to inspect the exact prompt sent to the
/// contextual extraction path.
#[cfg(any(test, feature = "test-support"))]
pub async fn run_llm_extraction_with_terminal_context_and_provider(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: SlotContext,
    terminal_context: TerminalExtractionContext,
    provider: Arc<dyn LlmProvider>,
) {
    run_llm_extraction_inner(
        session_id,
        taxonomy,
        app_state,
        terminal_context,
        Some(provider),
        None,
    )
    .await;
}

/// Test-only compatibility entry point that supplies unknown terminal context.
#[cfg(any(test, feature = "test-support"))]
pub async fn run_llm_extraction_with_provider(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: SlotContext,
    provider: Arc<dyn LlmProvider>,
) {
    run_llm_extraction_inner(
        session_id,
        taxonomy,
        app_state,
        TerminalExtractionContext::unknown_historical(),
        Some(provider),
        None,
    )
    .await;
}

/// Test-only compatibility entry point that supplies unknown terminal context.
#[cfg(any(test, feature = "test-support"))]
pub async fn run_llm_extraction_with_provider_and_candidate_lookup(
    session_id: String,
    taxonomy: SessionTaxonomy,
    app_state: SlotContext,
    provider: Arc<dyn LlmProvider>,
    candidate_lookup_override: CandidateLookupOverride,
) {
    run_llm_extraction_inner(
        session_id,
        taxonomy,
        app_state,
        TerminalExtractionContext::unknown_historical(),
        Some(provider),
        Some(candidate_lookup_override),
    )
    .await;
}

/// Capture replay decisions without creating or mutating notes.
///
/// The offline runner supplies the extraction completion it received from its
/// injected provider and the same bounded candidates the production lookup
/// returned. This deliberately reuses the production parser, intra-batch
/// deduplication, ADR-054 gate, and novelty request/response contract while
/// stopping before any persistence path.
#[cfg(any(test, feature = "test-support"))]
pub async fn capture_llm_extraction_replay(
    fixture_id: String,
    extraction_response: &str,
    provider: &dyn LlmProvider,
    candidates: &[djinn_db::NoteDedupCandidate],
) -> Result<Vec<crate::extraction_replay_eval::ExtractionObservation>, String> {
    let extracted = parse_extraction_response(extraction_response)?;
    let (notes, _) = dedup_extracted_notes(&extracted);
    let mut observations = Vec::with_capacity(notes.len());
    for (note_type, note) in notes {
        let quality_passed = !assess_note_quality(note_type, &note.content).is_underspecified;
        let duplicate_of = if quality_passed {
            // Replay takes the same malformed-novelty fallback as persisted
            // extraction while stopping before all note mutations.
            novelty_with_unknown_fallback(
                novelty_decision_for_candidates(provider, note_type, &note, candidates).await,
            )
            .existing_note_id
        } else {
            None
        };
        observations.push(crate::extraction_replay_eval::ExtractionObservation {
            fixture_id: fixture_id.clone(),
            note_type: note_type.to_string(),
            title: note.title.clone(),
            content: note.content.clone(),
            adr_054_quality_passed: quality_passed,
            duplicate_of,
        });
    }
    Ok(observations)
}

/// Inner implementation that accepts an optional provider override for test injection.
///
/// When `provider_override` is `Some`, the given provider is used directly
/// instead of resolving a memory provider.
async fn run_llm_extraction_inner(
    session_id: String,
    mut taxonomy: SessionTaxonomy,
    app_state: SlotContext,
    terminal_context: TerminalExtractionContext,
    provider_override: Option<Arc<dyn LlmProvider>>,
    #[cfg(any(test, feature = "test-support"))] candidate_lookup_override: Option<
        CandidateLookupOverride,
    >,
) {
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
    // Since migration 14, `sessions.project_id` is NULL for chat sessions.
    // This extractor only runs for task-scoped (non-chat) sessions, but guard
    // defensively: without a project_id there's nothing to extract against.
    let session_project_id = match session.project_id.as_deref() {
        Some(p) => p,
        None => {
            tracing::debug!(
                session_id = %session_id,
                "llm_extraction: session has no project_id (chat?); skipping"
            );
            return;
        }
    };
    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let project = match project_repo.get(session_project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::debug!(
                session_id = %session_id,
                project_id = %session_project_id,
                "llm_extraction: project not found; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                project_id = %session_project_id,
                error = %e,
                "llm_extraction: failed to load project; skipping"
            );
            return;
        }
    };
    // In tests, a provider_override bypasses credential loading entirely.
    let provider_override_present = provider_override.is_some();
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
        // Resolve the memory model the way DISPATCH does — act as the task's
        // creator (the owning user) so extraction uses THEIR
        // configured model + credential, like other system-initiated LLM work. The
        // session already ran on `session.model_id` for this user, so resolving
        // it under the creator's `SESSION_USER_ID` scope reuses the proven
        // provider path: e.g. an `openai/*` id served via the connected
        // `chatgpt_codex` credential. If creator-scoped resolution fails, fall
        // back only to the explicit org-shared/no-user memory-provider scope;
        // never borrow another user's private credential.
        match resolve_creator_scoped_llm_extraction_provider(
            &app_state,
            &session_id,
            &task_id,
            task.created_by_user_id.clone(),
            &session.model_id,
        )
        .await
        {
            LlmExtractionProviderResolution::Provider(provider) => provider,
            LlmExtractionProviderResolution::NoProvider { .. } => return,
        }
    };
    // B5a: knowledge extraction (the extraction completion + the per-note
    // novelty judgements) is a cheap background distillation, not the agent's
    // reasoning loop. Force the weakest reasoning tier so it doesn't waste
    // deep-thinking tokens/latency. `with_reasoning_effort` returns `None` for
    // config-less providers (e.g. test mocks), in which case we keep the
    // provider unchanged.
    let provider: Box<dyn LlmProvider> =
        match provider.with_reasoning_effort(djinn_provider::provider::ReasoningEffort::Minimal) {
            Some(downgraded) => downgraded,
            None => provider,
        };
    // Non-panicking fallback: `unwrap_or_else` provides a minimal `"{}"` payload if
    // serialization fails, so prompt construction never aborts the extraction.
    let taxonomy_json = serde_json::to_string(&taxonomy).unwrap_or_else(|_| "{}".to_string());
    // Load the actual conversation so the LLM has real content to distill —
    // the taxonomy is only event counts. Best-effort: an empty excerpt just
    // means the LLM falls back to counts (the prior behaviour).
    let transcript = {
        let msg_repo = djinn_db::SessionMessageRepository::new(
            app_state.db.clone(),
            app_state.event_bus.clone(),
        );
        match msg_repo.load_conversation(&session_id).await {
            Ok(conv) => build_transcript_excerpt(&conv.messages, TRANSCRIPT_EXCERPT_CHARS),
            Err(e) => {
                tracing::debug!(
                    session_id = %session_id,
                    error = %e,
                    "llm_extraction: could not load transcript for prompt; using counts only"
                );
                String::new()
            }
        }
    };
    let project_path_buf =
        djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
    let project_path = project_path_buf.to_string_lossy();
    let session_scope_paths =
        crate::session_extraction::derive_scope_paths(&taxonomy.changed_file_paths, &project_path);
    // Non-panicking fallback: `"[]"` is the safe empty scope set.
    let scope_json =
        serde_json::to_string(&session_scope_paths).unwrap_or_else(|_| "[]".to_string());
    let prompt = build_extraction_prompt(
        &task.title,
        &task.description,
        &taxonomy_json,
        &transcript,
        &scope_json,
        &terminal_context,
    );
    let completion = tokio::time::timeout(
        EXTRACTION_LLM_TIMEOUT,
        complete(
            provider.as_ref(),
            CompletionRequest {
                system: EXTRACTION_SYSTEM_PROMPT.to_string(),
                prompt,
                max_tokens: EXTRACTION_MAX_TOKENS,
            },
        ),
    )
    .await;
    let response = match completion {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "llm_extraction: LLM completion failed; skipping extraction"
            );
            let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            finalize_extraction_output(
                &note_repo,
                &project.id,
                &session_id,
                &task.id,
                session.task_run_id.as_deref(),
                0,
            )
            .await;
            return;
        }
        Err(_) => {
            tracing::warn!(
                session_id = %session_id,
                timeout_secs = EXTRACTION_LLM_TIMEOUT.as_secs(),
                "llm_extraction: LLM completion timed out; skipping extraction"
            );
            let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            finalize_extraction_output(
                &note_repo,
                &project.id,
                &session_id,
                &task.id,
                session.task_run_id.as_deref(),
                0,
            )
            .await;
            return;
        }
    };
    // FAILURE case: the call returned text but it could not be parsed as the
    // expected JSON shape. This is an error (the model misbehaved, the output
    // was truncated, etc.) and must be logged at warn — it is NOT the same as a
    // legitimately-empty extraction (handled below at debug).
    let extracted = match parse_extraction_response(&response.text) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                raw_response = %response.text,
                "llm_extraction: LLM response parse FAILED; skipping (extraction error, not empty)"
            );
            let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            finalize_extraction_output(
                &note_repo,
                &project.id,
                &session_id,
                &task.id,
                session.task_run_id.as_deref(),
                0,
            )
            .await;
            return;
        }
    };
    // The model can emit the same note more than once within a single
    // extraction (e.g. a case and a pitfall with an identical title, or two
    // copies of the same case). Collapse them by a normalized key
    // (lowercase+trimmed title + note_type) BEFORE any DB work so the same
    // note isn't created twice from one extraction. The cross-session /
    // semantic dedup against existing notes still happens later per-note.
    let (deduped_notes, intra_batch_dupes) = dedup_extracted_notes(&extracted);
    taxonomy.extraction_quality.dedup_skipped += intra_batch_dupes as u32;
    let total = deduped_notes.len();
    taxonomy.extraction_quality.extracted = total as u32;
    // EMPTY (success) case: the call + parse succeeded, but after dedup there is
    // nothing novel to record. This is normal — log at debug, not warn.
    if total == 0 {
        let repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        finalize_extraction_output(
            &repo,
            &project.id,
            &session_id,
            &task.id,
            session.task_run_id.as_deref(),
            0,
        )
        .await;
        persist_extraction_quality(&session_repo, &session_id, &taxonomy).await;
        tracing::debug!(
            session_id = %session_id,
            intra_batch_dupes,
            "llm_extraction: extraction succeeded but found nothing to record (empty, not a failure)"
        );
        return;
    }
    tracing::debug!(
        session_id = %session_id,
        cases = extracted.cases.len(),
        patterns = extracted.patterns.len(),
        pitfalls = extracted.pitfalls.len(),
        intra_batch_dupes,
        unique = total,
        "llm_extraction: writing extracted notes"
    );
    // Resolve the workspace path from the session's task_run.  Task #8
    // removed the `sessions.worktree_path` migration-window fallback; task
    // #13 will drop the column outright.
    let task_run_repo = TaskRunRepository::new(app_state.db.clone());
    let workspace_path: Option<String> = match session.task_run_id.as_deref() {
        Some(run_id) => task_run_repo
            .get(run_id)
            .await
            .ok()
            .flatten()
            .and_then(|run| run.workspace_path),
        None => None,
    };
    let knowledge_branch_target = app_state
        .knowledge_branch_target_for(Path::new(project_path.as_ref()), workspace_path.as_deref());
    tracing::debug!(
        session_id = %session_id,
        knowledge_branch_target = %knowledge_branch_target.intent_label(),
        worktree_root = ?knowledge_branch_target.worktree_root(),
        "llm_extraction: resolved knowledge write target"
    );
    // Notes are db-only — no on-disk mirror — so the knowledge_branch_target
    // worktree_root no longer routes file writes. The repo is constructed
    // without a worktree root; the embedding branch is set explicitly below.
    let note_repo = NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone())
        .with_embedding_branch(
            knowledge_branch_target
                .worktree_root()
                .and_then(djinn_db::infer_embedding_branch_from_worktree),
        );
    let provenance = format!(
        "\n\n---\n*Extracted from session {session_id}. Confidence: 0.5 (session-extracted).*"
    );
    let mut extraction_quality = taxonomy.extraction_quality.clone();
    let extraction_context = ExtractionContext {
        note_repo: &note_repo,
        provider: provider.as_ref(),
        project_id: &project.id,
        project_path: &project_path,
        knowledge_branch_target: &knowledge_branch_target,
        session_id: &session_id,
        task_id: &task.id,
        task_run_id: session.task_run_id.as_deref(),
        task_short_id: &task.short_id,
        task_title: &task.title,
        task_description: &task.description,
        provenance: &provenance,
        // Test providers are injected locally; production merge spend requires
        // the task creator attribution used by provider resolution.
        caller_attributed: provider_override_present || task.created_by_user_id.is_some(),
        session_scope_paths: &session_scope_paths,
        #[cfg(any(test, feature = "test-support"))]
        candidate_lookup: candidate_lookup_override
            .map(|lookup| CandidateLookup::with_override(lookup))
            .unwrap_or_else(CandidateLookup::production),
    };
    let mut durable_output_count = 0usize;
    for (note_type, note) in &deduped_notes {
        durable_output_count += process_extracted_note(
            &extraction_context,
            note_type,
            note,
            &mut extraction_quality,
        )
        .await;
    }
    finalize_extraction_output(
        &note_repo,
        &project.id,
        &session_id,
        &task.id,
        session.task_run_id.as_deref(),
        durable_output_count,
    )
    .await;
    taxonomy.extraction_quality = extraction_quality;
    persist_extraction_quality(&session_repo, &session_id, &taxonomy).await;
    // Write a lightweight consolidation_run_metrics row so the admission-dropped
    // count is queryable via memory_health and list_run_metrics. The row uses
    // note_type "extraction" and zeros for consolidation-specific fields. The
    // row is written even when admission_dropped == 0 so health() returns 0
    // (not NULL) for sessions with no drops.
    let now = now_rfc3339();
    let consolidation_repo = NoteConsolidationRepository::new(app_state.db.clone());
    if let Err(error) = consolidation_repo
        .create_run_metric(CreateConsolidationRunMetric {
            project_id: &project.id,
            note_type: "extraction",
            status: "completed",
            scanned_note_count: deduped_notes.len() as i64,
            candidate_cluster_count: 0,
            consolidated_cluster_count: 0,
            consolidated_note_count: taxonomy.extraction_quality.written as i64,
            source_note_count: 0,
            decayed_note_count: 0,
            archived_note_count: 0,
            superseded_source_note_count: 0,
            admission_dropped_note_count: taxonomy.extraction_quality.admission_dropped as i64,
            started_at: &now,
            completed_at: Some(&now),
            error_message: None,
        })
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %error,
            "llm_extraction: failed to write admission-dropped metric row"
        );
    }
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

/// Return the current UTC time as an RFC 3339 string.
///
/// Formatting a UTC `OffsetDateTime` as RFC 3339 is infallible in practice.
/// If it ever fails, fall back to the Unix epoch so the extraction path never
/// panics.
fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Merge only session-originated, non-curated candidates. The 52t1 revision-aware
/// update chokepoint is absent on this branch (the repository exposes `update`),
/// so this helper is the single extraction persistence seam for later revision instrumentation.
async fn persist_merged_extraction_content(
    context: &ExtractionContext<'_>,
    existing: &djinn_memory::Note,
    content: &str,
) -> djinn_db::Result<djinn_db::NoteRevisionMutationResult> {
    context
        .mutate_existing(
            existing,
            content.to_owned(),
            existing.confidence,
            NoteRevisionEventKind::Updated,
            "merged extracted evidence into existing note",
        )
        .await
}

fn split_provenance_footer(content: &str) -> (&str, Option<&str>) {
    let marker = "\n\n---\n*Extracted from session ";
    match content.find(marker) {
        Some(index) if content[index..].ends_with("(session-extracted).*") => {
            (&content[..index], Some(&content[index..]))
        }
        _ => (content, None),
    }
}

fn content_with_one_provenance_footer(
    model_content: &str,
    existing_content: &str,
    fallback: &str,
) -> String {
    let (model_body, _) = split_provenance_footer(model_content);
    let (_, existing_footer) = split_provenance_footer(existing_content);
    format!(
        "{}{}",
        model_body.trim_end(),
        existing_footer.unwrap_or(fallback)
    )
}

fn eligible_evidence_merge(note: &djinn_memory::Note, caller_attributed: bool) -> bool {
    caller_attributed
        && note.confidence < EVIDENCE_MERGE_MAX_CONFIDENCE
        && split_provenance_footer(&note.content).1.is_some()
}

async fn boost_duplicate_confidence(
    context: &ExtractionContext<'_>,
    candidate_id: &str,
    note_type: &str,
    title: &str,
    outcome: &str,
) -> bool {
    let existing = match context.note_repo.get(candidate_id).await {
        Ok(Some(note)) => note,
        Ok(None) | Err(_) => return false,
    };
    let updated_confidence = (existing.confidence * DUPLICATE_CONFIDENCE_SIGNAL)
        / (existing.confidence * DUPLICATE_CONFIDENCE_SIGNAL
            + (1.0 - existing.confidence) * (1.0 - DUPLICATE_CONFIDENCE_SIGNAL));
    if updated_confidence == existing.confidence {
        return false;
    }
    match context
        .mutate_existing(
            &existing,
            existing.content.clone(),
            updated_confidence,
            NoteRevisionEventKind::ConfidenceChanged,
            "confirmed duplicate extracted knowledge",
        )
        .await
    {
        Ok(result) => {
            tracing::debug!(session_id = %context.session_id, note_type, title, existing_note_id = candidate_id, updated_confidence, outcome, "llm_extraction: duplicate confidence updated");
            result.changed
        }
        Err(error) => {
            tracing::warn!(session_id = %context.session_id, note_type, title, existing_note_id = candidate_id, %error, outcome, "llm_extraction: duplicate confidence update failed");
            false
        }
    }
}

async fn merge_duplicate_evidence(
    context: &ExtractionContext<'_>,
    note: &ExtractedNote,
    selected: Option<&djinn_db::NoteDedupCandidate>,
) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    let existing = match context.note_repo.get(&selected.id).await {
        Ok(Some(note)) => note,
        Ok(None) | Err(_) => return 0,
    };
    if !eligible_evidence_merge(&existing, context.caller_attributed) {
        return 0;
    }
    let prompt = format!(
        "Existing full note body:\n{}\n\nFresh extracted evidence:\n{}\n\nReturn JSON only: {{\"content\":\"merged markdown body\"}}. Preserve concrete evidence from both bodies; do not wholesale replace the existing note. The session provenance footer is managed by the caller.",
        selected.content, note.content
    );
    let response = match complete(
        context.provider,
        CompletionRequest {
            system: EVIDENCE_MERGE_SYSTEM_PROMPT.to_string(),
            prompt,
            max_tokens: 800,
        },
    )
    .await
    {
        Ok(response) => response,
        Err(_) => return 0,
    };
    let merged: EvidenceMergeResponse =
        match serde_json::from_str::<EvidenceMergeResponse>(response.text.trim()) {
            Ok(merged) if !merged.content.trim().is_empty() => merged,
            _ => return 0,
        };
    let content =
        content_with_one_provenance_footer(&merged.content, &existing.content, context.provenance);
    // Persistence completes before the confidence signal; failure reaches boost-only fallback.
    match persist_merged_extraction_content(context, &existing, &content).await {
        Ok(result) => {
            let confidence_changed = boost_duplicate_confidence(
                context,
                &existing.id,
                "merge",
                &note.title,
                "evidence_merged",
            )
            .await;
            usize::from(result.changed) + usize::from(confidence_changed)
        }
        Err(_) => 0,
    }
}

async fn process_extracted_note(
    extraction_context: &ExtractionContext<'_>,
    note_type: &str,
    note: &ExtractedNote,
    extraction_quality: &mut super::session_extraction::ExtractionQuality,
) -> usize {
    // Runs BEFORE the novelty judge and BEFORE `create_extracted_note`. A
    // candidate that fails the structural gate is dropped without a novelty
    // LLM call, without a working-spec fallback, and without any note write
    // (neither `case`/`pattern`/`pitfall` nor `design` working spec). The
    // `is_underspecified` decision is delegated to the shared
    // `assess_note_quality` classifier so this gate and the corpus audit
    // (graph.rs::extracted_note_audit) cannot drift. The gate is scoped to
    // `run_llm_extraction_inner`; human-authored memory writes are
    // unaffected.
    if matches!(note_type, "case" | "pattern" | "pitfall") {
        let quality = assess_note_quality(note_type, &note.content);
        if quality.is_underspecified {
            extraction_quality.admission_dropped += 1;
            tracing::warn!(
                session_id = %extraction_context.session_id,
                project_id = %extraction_context.project_id,
                note_type = %note_type,
                title = %note.title,
                reasons = ?quality.reasons,
                "llm_extraction: dropping underspecified note at admission gate"
            );
            return 0;
        }
    }
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
            novelty_with_unknown_fallback(Err(e))
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
        required_structure = assessment.required_structure,
        reasons = ?assessment.reasons,
        "llm_extraction: evaluated extraction quality gate"
    );
    match assessment.outcome {
        ExtractionOutcome::MergeIntoExisting => {
            if let Some(candidate_id) = novelty.existing_note_id.as_deref() {
                // Quality accounting describes the semantic dedup decision, not
                // whether its canonical content/confidence mutation committed.
                // Keep these counters independent from the durable-output count
                // returned below; terminal skipped routing uses only that count.
                extraction_quality.novelty_skipped += 1;
                extraction_quality.merged += 1;
                let durable_outputs = merge_duplicate_evidence(
                    extraction_context,
                    note,
                    novelty.selected_candidate.as_ref(),
                )
                .await;
                if durable_outputs == 0 {
                    let boosted = boost_duplicate_confidence(
                        extraction_context,
                        candidate_id,
                        note_type,
                        &note.title,
                        "boost_fallback",
                    )
                    .await;
                    extraction_quality.boost_fallback += 1;
                    tracing::debug!(
                        session_id = %extraction_context.session_id,
                        note_type = %note_type,
                        title = %note.title,
                        existing_note_id = %candidate_id,
                        outcome = "boost_fallback",
                        "llm_extraction: already-known decision completed with confidence-only fallback"
                    );
                    return usize::from(boosted);
                } else {
                    extraction_quality.evidence_merged += 1;
                    tracing::debug!(
                        session_id = %extraction_context.session_id,
                        note_type = %note_type,
                        title = %note.title,
                        existing_note_id = %candidate_id,
                        outcome = "evidence_merged",
                        "llm_extraction: already-known decision completed with evidence merge"
                    );
                }
                return durable_outputs;
            }
            return 0;
        }
        ExtractionOutcome::DowngradeToWorkingSpec => {
            let changed = persist_working_spec(extraction_context, note, &assessment.reasons).await;
            extraction_quality.downgraded += 1;
            return usize::from(changed);
        }
        ExtractionOutcome::Discard => {
            extraction_quality.discarded += 1;
            return 0;
        }
        ExtractionOutcome::DurableWrite => {}
    }
    let content_with_provenance = format!("{}{}", note.content, extraction_context.provenance);
    let scope_paths = if note.scope_paths.is_empty() {
        extraction_context.session_scope_paths.to_vec()
    } else {
        note.scope_paths.clone()
    };
    // Non-panicking fallback: `"[]"` is the safe empty scope set.
    let scope_paths_json = serde_json::to_string(&scope_paths).unwrap_or_else(|_| "[]".to_string());
    let retrieval_anchor = note.normalized_anchor();
    if note
        .retrieval_anchor
        .as_deref()
        .map(str::trim)
        .map(str::is_empty)
        .unwrap_or(false)
    {
        // Model emitted an anchor that was empty after whitespace trimming.
        // The field is optional, so we accept the note without it — debug only.
        tracing::debug!(
            session_id = %extraction_context.session_id,
            note_type = %note_type,
            title = %note.title,
            "llm_extraction: retrieval anchor was empty after trim; writing note without anchor"
        );
    }
    match extraction_context
        .create_extracted_note(
            &note.title,
            &content_with_provenance,
            note_type,
            &scope_paths_json,
            retrieval_anchor.as_deref(),
        )
        .await
    {
        Ok(result) => {
            let created_id = result
                .note
                .as_ref()
                .map(|note| note.id.as_str())
                .unwrap_or("unknown");
            tracing::debug!(
                session_id = %extraction_context.session_id,
                note_id = %created_id,
                note_type = %note_type,
                title = %note.title,
                "llm_extraction: note created"
            );
            extraction_quality.written += 1;
            return 1;
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
    0
}

async fn persist_working_spec(
    extraction_context: &ExtractionContext<'_>,
    note: &ExtractedNote,
    reasons: &[&'static str],
) -> bool {
    let scope_paths = if note.scope_paths.is_empty() {
        extraction_context.session_scope_paths.to_vec()
    } else {
        note.scope_paths.clone()
    };
    // Non-panicking fallback: `"[]"` is the safe empty scope set.
    let scope_paths_json = serde_json::to_string(&scope_paths).unwrap_or_else(|_| "[]".to_string());
    let title = format!("Working Spec {}", extraction_context.task_short_id);
    let permalink = permalink_for("design", &title);
    let section = render_working_spec_entry(extraction_context, note, reasons, &scope_paths);
    match extraction_context
        .note_repo
        .get_by_permalink(extraction_context.project_id, &permalink)
        .await
    {
        Ok(Some(existing)) => {
            let merged = merge_working_spec_content(&existing.content, &section);
            match extraction_context
                .mutate_existing(
                    &existing,
                    merged.clone(),
                    existing.confidence,
                    NoteRevisionEventKind::Updated,
                    "updated extraction working specification",
                )
                .await
            {
                Ok(result) => {
                    let updated_id = result
                        .note
                        .as_ref()
                        .map(|note| note.id.as_str())
                        .unwrap_or("unknown");
                    tracing::debug!(
                        session_id = %extraction_context.session_id,
                        note_id = %updated_id,
                        permalink = %permalink,
                        "llm_extraction: updated task working spec"
                    );
                    return result.changed;
                }
                Err(error) => tracing::warn!(
                    session_id = %extraction_context.session_id,
                    permalink = %permalink,
                    error = %error,
                    "llm_extraction: failed to update working spec"
                ),
            }
        }
        Ok(None) => match extraction_context
            .create_extracted_note(
                &title,
                &render_working_spec_document(extraction_context, &section, &scope_paths),
                "design",
                &scope_paths_json,
                None,
            )
            .await
        {
            Ok(result) => {
                let created_id = result
                    .note
                    .as_ref()
                    .map(|note| note.id.as_str())
                    .unwrap_or("unknown");
                tracing::debug!(
                session_id = %extraction_context.session_id,
                note_id = %created_id,
                permalink = %permalink,
                "llm_extraction: created task working spec"
                );
                return true;
            }
            Err(error) => tracing::warn!(
                session_id = %extraction_context.session_id,
                permalink = %permalink,
                error = %error,
                "llm_extraction: failed to create working spec"
            ),
        },
        Err(error) => tracing::warn!(
            session_id = %extraction_context.session_id,
            permalink = %permalink,
            error = %error,
            "llm_extraction: failed to load existing working spec"
        ),
    }
    false
}

fn render_working_spec_document(
    extraction_context: &ExtractionContext<'_>,
    section: &str,
    scope_paths: &[String],
) -> String {
    let scope_lines = if scope_paths.is_empty() {
        "- none captured".to_string()
    } else {
        scope_paths
            .iter()
            .map(|path| format!("- `{path}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "# Working Spec\n\n## Active objective\n- Task {task_short_id}: {task_title}\n- {task_description}\n\n## Relevant scope\n{scope_lines}\n\n## Constraints\n- This note is task-scoped working context routed from non-durable extraction output.\n- Keep mutable hypotheses and open questions here instead of promoting them to durable case/pattern/pitfall notes.\n\n## Current hypotheses\n- Session-local understanding may evolve as implementation continues.\n\n## Open questions\n- Which parts of this working context should be promoted or discarded when the task completes?\n\n## Captured session knowledge\n{section}",
        task_short_id = extraction_context.task_short_id,
        task_title = extraction_context.task_title,
        task_description = extraction_context.task_description,
    )
}

fn render_working_spec_entry(
    extraction_context: &ExtractionContext<'_>,
    note: &ExtractedNote,
    reasons: &[&'static str],
    scope_paths: &[String],
) -> String {
    let routing_reasons = if reasons.is_empty() {
        "- session_local_context".to_string()
    } else {
        reasons
            .iter()
            .map(|reason| format!("- {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let scope_lines = if scope_paths.is_empty() {
        "- none captured".to_string()
    } else {
        scope_paths
            .iter()
            .map(|path| format!("- `{path}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "### {title}\n\n#### Objective\n- Preserve useful but non-durable understanding for task {task_short_id}.\n\n#### Files / symbols / scope\n{scope_lines}\n\n#### Constraints\n{routing_reasons}\n\n#### Current hypotheses\n- {content}\n\n#### Open questions\n- Should any portion of this be promoted into durable memory after the task completes?\n\n#### Routing rationale\n- Routed from extracted session output because it was useful for the current task but failed durable extraction thresholds.\n\n#### Provenance\n- Extracted from session {session_id}.\n",
        title = note.title,
        task_short_id = extraction_context.task_short_id,
        content = note.content.trim(),
        session_id = extraction_context.session_id,
    )
}

fn merge_working_spec_content(existing: &str, section: &str) -> String {
    let trimmed_existing = existing.trim_end();
    let trimmed_section = section.trim();
    if trimmed_existing.contains(trimmed_section) {
        // Preserve the committed representation exactly on a semantic no-op;
        // trimming a terminal newline would otherwise fabricate a revision.
        existing.to_owned()
    } else {
        format!("{trimmed_existing}\n\n{trimmed_section}\n")
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
    let result =
        novelty_decision_for_candidates(extraction_context.provider, note_type, note, &candidates)
            .await?;
    if let Some(existing_note_id) = result.existing_note_id.as_deref() {
        tracing::debug!(
            session_id = %extraction_context.session_id,
            note_type = %note_type,
            title = %note.title,
            existing_note_id = %existing_note_id,
            "llm_extraction: semantic duplicate decision returned already_known"
        );
    }
    Ok(result)
}

/// Execute the production novelty request and validate its response against the
/// bounded candidate set. Persisted extraction and replay share this decision
/// path so their prompt, JSON handling, and candidate-ID validation match.
async fn novelty_decision_for_candidates(
    provider: &dyn LlmProvider,
    note_type: &str,
    note: &ExtractedNote,
    candidates: &[djinn_db::NoteDedupCandidate],
) -> Result<NoveltyCheckResult, String> {
    if candidates.is_empty() {
        return Ok(novelty_result(NoveltyAssessment::Novel));
    }
    let candidate_abstract = summarize_candidate_note(note);
    let response = complete(
        provider,
        CompletionRequest {
            system: NOVELTY_SYSTEM_PROMPT.to_string(),
            prompt: build_novelty_prompt(note_type, note, &candidate_abstract, candidates),
            max_tokens: 300,
        },
    )
    .await
    .map_err(|e| format!("semantic compare failed: {e}"))?;
    let decision: NoveltyDecision = serde_json::from_str(response.text.trim())
        .map_err(|e| format!("invalid novelty decision json: {e}"))?;
    match decision.decision {
        NoveltyDecisionKind::Novel => Ok(novelty_result(NoveltyAssessment::Novel)),
        NoveltyDecisionKind::AlreadyKnown => {
            let existing_note_id = decision
                .existing_note_id
                .filter(|id| candidates.iter().any(|candidate| candidate.id == *id))
                .ok_or_else(|| {
                    "already_known decision missing valid existing_note_id".to_string()
                })?;
            Ok(NoveltyCheckResult {
                assessment: NoveltyAssessment::Duplicate,
                existing_note_id: Some(existing_note_id.clone()),
                selected_candidate: candidates
                    .iter()
                    .find(|candidate| candidate.id == existing_note_id)
                    .cloned(),
            })
        }
    }
}

fn novelty_result(assessment: NoveltyAssessment) -> NoveltyCheckResult {
    NoveltyCheckResult {
        assessment,
        existing_note_id: None,
        selected_candidate: None,
    }
}

/// Preserve the production outcome for unavailable or invalid novelty results:
/// continue quality/outcome evaluation with unknown novelty.
fn novelty_with_unknown_fallback(result: Result<NoveltyCheckResult, String>) -> NoveltyCheckResult {
    result.unwrap_or_else(|_| novelty_result(NoveltyAssessment::Unknown))
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
    // The ADR-054 structural gate now delegates to the shared
    // `assess_note_quality` classifier (the single source of truth shared with
    // `extracted_note_audit`), so the gate and corpus audit cannot drift.
    let quality = assess_note_quality(note_type, &note.content);
    let required_structure = !quality.is_underspecified;
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
    if !required_structure {
        reasons.push("missing_required_adr_054_sections");
    }
    if novelty_assessment == NoveltyAssessment::Duplicate {
        reasons.push("semantic_duplicate_of_existing_note");
    }
    let outcome = if novelty_assessment == NoveltyAssessment::Duplicate {
        ExtractionOutcome::MergeIntoExisting
    } else if !required_structure {
        ExtractionOutcome::DowngradeToWorkingSpec
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
        required_structure,
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
    #[cfg(any(test, feature = "test-support"))]
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

/// Truncate an existing candidate body to a deterministic Unicode-safe cap for
/// the novelty prompt. The repository still carries the full body; this only
/// bounds what the LLM sees.
fn truncate_novelty_candidate_content(content: &str) -> String {
    let mut characters = content.chars();
    let body: String = characters
        .by_ref()
        .take(NOVELTY_CANDIDATE_CONTENT_CHAR_CAP)
        .collect();
    if characters.next().is_some() {
        format!("{body}\n… [truncated at {NOVELTY_CANDIDATE_CONTENT_CHAR_CAP} characters]")
    } else {
        body
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
            let body = truncate_novelty_candidate_content(&candidate.content);
            format!(
                "- id: {}\n  title: {}\n  body:\n{}\n  summary: {}",
                candidate.id, candidate.title, body, summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Note type: {note_type}\nProposed extracted note title: {title}\nProposed extracted note summary:\n{candidate_abstract}\n\nExisting candidates:\n{candidate_lines}\n\nReturn JSON only in this schema:\n{{\"decision\":\"already_known\"|\"novel\",\"existing_note_id\":\"candidate-id-or-null\"}}\nChoose already_known only when the proposed note is semantically the same knowledge as one existing candidate. Otherwise choose novel.",
        title = note.title,
    )
}

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
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("JSON parse error: {error}"))?;
    let has_revision_operations = value
        .as_object()
        .is_some_and(|object| object.contains_key("revision_operations"));
    let mut extracted: ExtractionResponse = serde_json::from_value(value).map_err(|error| {
        if has_revision_operations {
            RevisionOperationRefusalReason::MalformedOperationShape.to_string()
        } else {
            format!("JSON parse error: {error}")
        }
    })?;
    validate_revision_operations(&mut extracted.revision_operations)
        .map_err(|reason| reason.to_string())?;
    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_extraction::ExtractionQuality;
    use crate::test_helpers::{agent_context_from_db, create_test_db, test_path};
    use djinn_db::NoteDedupCandidate;
    use tokio_util::sync::CancellationToken;
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl CapturedLogs {
        fn take(&self) -> String {
            let mut buf = self.0.lock().expect("captured logs mutex poisoned");
            let out =
                String::from_utf8(buf.clone()).expect("captured log bytes were not valid utf-8");
            buf.clear();
            out
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogsWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogsWriter {
                inner: std::sync::Arc::clone(&self.0),
            }
        }
    }
    struct CapturedLogsWriter {
        inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl std::io::Write for CapturedLogsWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner
                .lock()
                .expect("captured logs mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn extraction_telemetry_for_test(
        task_id: &str,
        creator: Option<&str>,
    ) -> djinn_provider::provider::TelemetryMeta {
        crate::helpers::build_telemetry_meta_with_attribution(
            "memory_extraction",
            task_id,
            Some("memory_extraction"),
            creator,
        )
    }
    struct CredentialScopedTestCallbacks;
    impl crate::host::SlotHostCallbacks for CredentialScopedTestCallbacks {
        fn interrupt_paused_worker_session<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
        fn resolve_mcp_tools<'a>(
            &'a self,
            _worktree_path: &'a str,
            _role_name: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::host::ResolvedMcpTools, String>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err("not implemented in credential-scoped test".into()) })
        }
        fn render_prompt(
            &self,
            _role_name: &str,
            _task: &djinn_core::models::Task,
            _context_json: &serde_json::Value,
        ) -> String {
            String::new()
        }
        fn initial_user_message<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async { String::new() })
        }
        fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
            panic!("not implemented in credential-scoped test")
        }
        fn require_project_id_for_task_ops<'a>(
            &'a self,
            _project: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            String,
                            djinn_control_plane::tools::task_tools::ErrorResponse,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                    error: "not implemented in credential-scoped test".into(),
                })
            })
        }
        fn resolve_provider_credential<'a>(
            &'a self,
            provider_id: &'a str,
            ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::helpers::ProviderCredential, String>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                let key_name = ctx
                    .catalog
                    .list_providers()
                    .into_iter()
                    .find(|provider| provider.id == provider_id)
                    .and_then(|provider| provider.env_vars.into_iter().next())
                    .unwrap_or_else(|| format!("{}_API_KEY", provider_id.to_ascii_uppercase()));
                let credential_repo = djinn_provider::repos::CredentialRepository::new(
                    ctx.db.clone(),
                    ctx.event_bus.clone(),
                );
                match credential_repo
                    .get_decrypted(&key_name)
                    .await
                    .map_err(|error| format!("credential lookup failed: {error}"))?
                {
                    Some(value) => Ok(crate::helpers::ProviderCredential::ApiKey(key_name, value)),
                    None => Err(format!(
                        "no credential stored for provider {provider_id} (expected key {key_name})"
                    )),
                }
            })
        }
        fn run_task_dispatch<'a>(
            &'a self,
            _task_id: String,
            _project_path: String,
            _model_id: String,
            _ctx: SlotContext,
            _kill: tokio_util::sync::CancellationToken,
            _pause: tokio_util::sync::CancellationToken,
            _resume_lifecycle_metadata: Option<serde_json::Value>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
        fn touch_activity_rpc<'a>(
            &'a self,
            _task_id: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
        fn flush_session_tokens_rpc<'a>(
            &'a self,
            _session_id: String,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }
    fn credential_scoped_test_context(db: djinn_db::Database) -> SlotContext {
        let mut ctx = agent_context_from_db(db, CancellationToken::new());
        ctx.callbacks = std::sync::Arc::new(CredentialScopedTestCallbacks);
        ctx
    }
    fn provider_auth_key(config: &djinn_provider::provider::ProviderConfig) -> Option<&str> {
        match &config.auth {
            djinn_provider::provider::AuthMethod::BearerToken(key) => Some(key),
            djinn_provider::provider::AuthMethod::ApiKeyHeader { key, .. } => Some(key),
            djinn_provider::provider::AuthMethod::NoAuth => None,
        }
    }
    async fn seed_credential_scope_users(db: djinn_db::Database) -> (String, String) {
        let users = djinn_db::UserRepository::new(db);
        let creator = users
            .upsert_from_github(710_001, "credential-scope-creator", None, None)
            .await
            .expect("seed creator user");
        let other = users
            .upsert_from_github(710_002, "credential-scope-other", None, None)
            .await
            .expect("seed other user");
        (creator.id, other.id)
    }
    #[tokio::test]
    async fn creator_scoped_llm_extraction_fails_closed_with_only_other_user_private_credential() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("initialize test db");
        let (creator_user_id, other_user_id) = seed_credential_scope_users(db.clone()).await;
        djinn_db::SettingsRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .set(
                "settings.raw",
                r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
            )
            .await
            .expect("configure memory model");
        djinn_provider::repos::CredentialRepository::new(
            db.clone(),
            djinn_core::events::EventBus::noop(),
        )
        .set_with_owner(
            "anthropic",
            "ANTHROPIC_API_KEY",
            "other-user-private-key",
            Some(&other_user_id),
        )
        .await
        .expect("seed other user's private credential");
        let ctx = credential_scoped_test_context(db);
        let resolution = resolve_creator_scoped_llm_extraction_provider(
            &ctx,
            "session-creator-absent",
            "task-creator-absent",
            Some(creator_user_id),
            "anthropic/claude-3-5-haiku-latest",
        )
        .await;
        match resolution {
            LlmExtractionProviderResolution::NoProvider { error, .. } => assert!(
                error.contains("no connected builtin provider models are available")
                    || error.contains("no credential stored"),
                "creator-scoped extraction should fail closed without creator/org-shared credentials: {error}"
            ),
            LlmExtractionProviderResolution::Provider(provider) => {
                let config = provider
                    .config_snapshot()
                    .expect("unexpected provider should expose config snapshot");
                assert_ne!(
                    provider_auth_key(&config),
                    Some("other-user-private-key"),
                    "creator-scoped extraction must never borrow another user's private key"
                );
                panic!(
                    "expected no provider when only another user's private credential exists, got {}",
                    provider.name()
                );
            }
        }
    }
    #[tokio::test]
    async fn creator_scoped_llm_extraction_uses_org_shared_fallback_not_other_user_private() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("initialize test db");
        let (creator_user_id, other_user_id) = seed_credential_scope_users(db.clone()).await;
        djinn_db::SettingsRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .set(
                "settings.raw",
                r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
            )
            .await
            .expect("configure memory model");
        let credential_repo = djinn_provider::repos::CredentialRepository::new(
            db.clone(),
            djinn_core::events::EventBus::noop(),
        );
        credential_repo
            .set_with_owner(
                "anthropic",
                "ANTHROPIC_API_KEY",
                "other-user-private-key",
                Some(&other_user_id),
            )
            .await
            .expect("seed other user's private credential");
        credential_repo
            .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "org-shared-key", None)
            .await
            .expect("seed org-shared credential");
        let ctx = credential_scoped_test_context(db);
        let resolution = resolve_creator_scoped_llm_extraction_provider(
            &ctx,
            "session-org-fallback",
            "task-org-fallback",
            Some(creator_user_id.clone()),
            "anthropic/claude-3-5-haiku-latest",
        )
        .await;
        let provider = match resolution {
            LlmExtractionProviderResolution::Provider(provider) => provider,
            LlmExtractionProviderResolution::NoProvider { error, .. } => {
                panic!("expected org-shared provider fallback, got error: {error}")
            }
        };
        assert_eq!(provider.name(), "anthropic");
        let config = provider
            .config_snapshot()
            .expect("resolved provider should expose config snapshot");
        assert_eq!(
            provider_auth_key(&config),
            Some("org-shared-key"),
            "extraction should use the explicit org-shared fallback credential"
        );
        assert_ne!(
            provider_auth_key(&config),
            Some("other-user-private-key"),
            "extraction must not use another user's private credential"
        );
        assert_eq!(
            config.telemetry.and_then(|telemetry| telemetry.user_id),
            Some(creator_user_id),
            "provider telemetry remains attributed to the extraction creator"
        );
    }
    #[tokio::test]
    async fn llm_extraction_fallback_returns_early_when_no_org_shared_provider() {
        use tracing::dispatcher::Dispatch;
        let db = djinn_db::Database::open_in_memory().expect("in-memory db");
        djinn_db::SettingsRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .set("settings.raw", r#"{"models":["openai/gpt-4.1-mini"]}"#)
            .await
            .expect("configure memory model without credentials");
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(logs.clone())
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
            .with_target(false)
            .with_ansi(false)
            .with_level(true)
            .finish();
        let dispatch = Dispatch::new(subscriber);
        let guard = tracing::dispatcher::set_default(&dispatch);
        let resolution = resolve_llm_extraction_provider_after_creator_attempt(
            &db,
            "session-no-provider",
            None,
            extraction_telemetry_for_test("task-no-provider", Some("user_a")),
        )
        .await;
        drop(guard);
        let captured = logs.take();
        assert!(
            captured.contains(NO_LLM_PROVIDER_WARNING),
            "fallback branch must emit the existing loud warning; captured: {captured}"
        );
        assert!(
            captured.contains("session-no-provider"),
            "warning should retain session context; captured: {captured}"
        );
        match resolution {
            LlmExtractionProviderResolution::NoProvider {
                warning_message,
                error,
            } => {
                assert_eq!(warning_message, NO_LLM_PROVIDER_WARNING);
                assert!(
                    error.contains("no connected builtin provider models are available"),
                    "fallback should fail before any completion-capable provider is returned: {error}"
                );
            }
            LlmExtractionProviderResolution::Provider(provider) => panic!(
                "expected no provider and therefore no possible LLM completion call, got {}",
                provider.name()
            ),
        }
    }
    #[tokio::test]
    async fn llm_extraction_fallback_uses_org_shared_provider_with_memory_telemetry() {
        let db = djinn_db::Database::open_in_memory().expect("in-memory db");
        djinn_db::SettingsRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .set(
                "settings.raw",
                r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
            )
            .await
            .expect("configure org-shared memory model");
        djinn_provider::repos::CredentialRepository::new(
            db.clone(),
            djinn_core::events::EventBus::noop(),
        )
        .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "org-key", None)
        .await
        .expect("configure org-shared credential");
        let resolution = resolve_llm_extraction_provider_after_creator_attempt(
            &db,
            "session-fallback",
            None,
            extraction_telemetry_for_test("task-fallback", Some("user_a")),
        )
        .await;
        let provider = match resolution {
            LlmExtractionProviderResolution::Provider(provider) => provider,
            LlmExtractionProviderResolution::NoProvider { error, .. } => {
                panic!("expected org-shared fallback provider, got error: {error}")
            }
        };
        assert_eq!(provider.name(), "anthropic");
        let config = provider
            .config_snapshot()
            .expect("org-shared fallback provider should expose config snapshot");
        let telemetry = config
            .telemetry
            .expect("fallback branch must attach memory extraction telemetry");
        assert_eq!(telemetry.user_id.as_deref(), Some("user_a"));
        assert_eq!(telemetry.operation.as_deref(), Some("memory_extraction"));
    }
    /// B5a: knowledge extraction is a cheap background distillation. The call
    /// site downgrades its resolved provider to the weakest reasoning tier
    /// before issuing the extraction + novelty completions. This locks the
    /// exact downgrade expression used in `run_llm_extraction_inner`.
    #[test]
    fn extraction_downgrades_provider_to_weakest_reasoning_tier() {
        use djinn_provider::provider::{
            AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig, ReasoningEffort,
            create_provider,
        };
        // A resolved provider as extraction would build it — start STRONG so a
        // missing/incorrect override is visible.
        let provider: Box<dyn LlmProvider> = create_provider(ProviderConfig {
            base_url: "https://example.test".to_string(),
            auth: AuthMethod::NoAuth,
            format_family: FormatFamily::Anthropic,
            model_id: "test-model".to_string(),
            context_window: 128_000,
            telemetry: None,
            session_affinity_key: None,
            provider_headers: Default::default(),
            capabilities: ProviderCapabilities::default(),
            reasoning_effort: Some(ReasoningEffort::High),
            tool_schema_compat: None,
        });
        // Exact downgrade logic from the call site.
        let provider: Box<dyn LlmProvider> =
            match provider.with_reasoning_effort(ReasoningEffort::Minimal) {
                Some(downgraded) => downgraded,
                None => provider,
            };
        assert_eq!(
            provider.config_snapshot().unwrap().reasoning_effort,
            Some(ReasoningEffort::Minimal),
            "extraction must run its LLM calls at the weakest reasoning tier"
        );
    }
    #[test]
    fn transcript_excerpt_renders_text_tools_and_results_and_skips_system() {
        use djinn_core::message::{ContentBlock, Message, Role};
        let messages = vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::text("you are a worker")],
                metadata: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::text("I'll fix the migrations path"),
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "edit".into(),
                        input: serde_json::json!({"file": "migrations.rs"}),
                    },
                ],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: vec![ContentBlock::text("No such file or directory")],
                    is_error: true,
                }],
                metadata: None,
            },
        ];
        let out = build_transcript_excerpt(&messages, 12_000);
        assert!(
            !out.contains("you are a worker"),
            "system prompt must be skipped"
        );
        assert!(out.contains("assistant: I'll fix the migrations path"));
        assert!(out.contains("assistant → edit("));
        assert!(out.contains("tool error: No such file or directory"));
    }
    #[test]
    fn transcript_excerpt_tail_biased_truncation() {
        use djinn_core::message::{ContentBlock, Message, Role};
        let messages: Vec<Message> = (0..200)
            .map(|i| Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text(format!("line {i}"))],
                metadata: None,
            })
            .collect();
        let out = build_transcript_excerpt(&messages, 200);
        assert!(
            out.len() <= 240,
            "should be capped near max_chars: {}",
            out.len()
        );
        assert!(out.contains("earlier turns omitted"));
        assert!(
            out.contains("line 199"),
            "tail (latest) content must be kept"
        );
        assert!(!out.contains("line 0:"), "head should be dropped");
    }
    #[test]
    fn parse_extraction_response_accepts_typed_revision_operations() {
        let patch_id = "018f0000-0000-7000-8000-000000000001";
        let replacement_id = "018f0000-0000-7000-8000-000000000002";
        let json = format!(
            r#"{{"cases":[{{"title":"T","content":"C"}}],"revision_operations":[{{"kind":"patch","target_note_id":"{patch_id}","before_text":"old","after_text":"new","confidence_delta":0.25,"reason":"  corrected evidence  "}},{{"kind":"deprecate_with_supersedes","deprecated_note_id":"{patch_id}","superseding_note_id":"{replacement_id}","reason":"replacement is authoritative"}}]}}"#
        );
        let response = parse_extraction_response(&json).expect("typed operations parse");
        assert_eq!(response.cases.len(), 1);
        assert_eq!(response.revision_operations.len(), 2);
        assert!(matches!(
            &response.revision_operations[0],
            RevisionOperation::Patch { reason, confidence_delta, .. }
                if reason == "corrected evidence" && *confidence_delta == 0.25
        ));
        assert!(matches!(
            &response.revision_operations[1],
            RevisionOperation::DeprecateWithSupersedes { .. }
        ));
    }

    #[test]
    fn parse_extraction_response_refuses_invalid_revision_operations() {
        let id = "018f0000-0000-7000-8000-000000000001";
        for (operation, expected) in [
            (
                r#"{"kind":"patch","target_note_id":"bad","before_text":"old","after_text":"new","confidence_delta":0.0,"reason":"why"}"#
                    .to_string(),
                "invalid_note_id",
            ),
            (
                format!(
                    r#"{{"kind":"patch","target_note_id":"{id}","before_text":"old","after_text":" ","confidence_delta":0.0,"reason":"why"}}"#
                ),
                "blank_required_text",
            ),
            (
                format!(
                    r#"{{"kind":"patch","target_note_id":"{id}","before_text":"old","after_text":"new","confidence_delta":0.26,"reason":"why"}}"#
                ),
                "confidence_delta_out_of_range",
            ),
            (
                format!(
                    r#"{{"kind":"deprecate_with_supersedes","deprecated_note_id":"{id}","superseding_note_id":"{id}","reason":"why"}}"#
                ),
                "self_replacement",
            ),
            (
                format!(
                    r#"{{"kind":"deprecate_with_supersedes","deprecated_note_id":"{id}","superseding_note_id":"018f0000-0000-7000-8000-000000000002","reason":" "}}"#
                ),
                "blank_reason",
            ),
            (
                r#"{"kind":"patch","target_note_id":"x"}"#.to_owned(),
                "malformed_operation_shape",
            ),
        ] {
            let json = format!(r#"{{"revision_operations":[{operation}]}}"#);
            assert_eq!(parse_extraction_response(&json).unwrap_err(), expected);
        }
    }

    #[test]
    fn revision_operations_do_not_participate_in_note_deduplication() {
        let response = parse_extraction_response(r#"{"cases":[{"title":"same","content":"one"},{"title":" SAME ","content":"two"}],"revision_operations":[{"kind":"patch","target_note_id":"018f0000-0000-7000-8000-000000000001","before_text":"old","after_text":"new","confidence_delta":0.0,"reason":"why"}]}"#).expect("response parses");
        let (notes, duplicates) = dedup_extracted_notes(&response);
        assert_eq!(notes.len(), 1);
        assert_eq!(duplicates, 1);
        assert_eq!(response.revision_operations.len(), 1);
    }

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
    fn parse_extraction_response_parses_applies_when_per_note() {
        // Prompt-facing field name `applies_when` must parse into the durable
        // `retrieval_anchor` slot.
        let json = r#"{
            "cases": [{"title":"T","content":"C","applies_when":"When T applies."}],
            "patterns": [{"title":"P","content":"P","applies_when":"When P applies."}],
            "pitfalls": [{"title":"F","content":"F","applies_when":"When F applies."}]
        }"#;
        let result = parse_extraction_response(json).expect("applies_when json parses");
        assert_eq!(
            result.cases[0].normalized_anchor().as_deref(),
            Some("When T applies.")
        );
        assert_eq!(
            result.patterns[0].normalized_anchor().as_deref(),
            Some("When P applies.")
        );
        assert_eq!(
            result.pitfalls[0].normalized_anchor().as_deref(),
            Some("When F applies.")
        );
    }
    #[test]
    fn parse_extraction_response_accepts_retrieval_anchor_alias() {
        // The storage-facing field name `retrieval_anchor` is also accepted as
        // a serde alias so existing call sites that already use it still work.
        let json = r#"{
            "cases": [{"title":"T","content":"C","retrieval_anchor":"When T applies."}],
            "patterns": [],
            "pitfalls": []
        }"#;
        let result = parse_extraction_response(json).expect("retrieval_anchor alias parses");
        assert_eq!(
            result.cases[0].normalized_anchor().as_deref(),
            Some("When T applies.")
        );
    }
    #[test]
    fn parse_extraction_response_tolerates_missing_applies_when() {
        // A model that forgets the field must not break extraction — the note
        // persists without an anchor (legacy behavior).
        let json = r#"{
            "cases": [{"title":"T","content":"C"}],
            "patterns": [],
            "pitfalls": []
        }"#;
        let result = parse_extraction_response(json).expect("missing anchor parses");
        assert_eq!(result.cases.len(), 1);
        assert!(result.cases[0].normalized_anchor().is_none());
    }
    #[test]
    fn parse_extraction_response_tolerates_empty_and_whitespace_anchor() {
        // Empty / whitespace-only anchors normalize to None and do not crash.
        let json = r#"{
            "cases": [
                {"title":"A","content":"A","applies_when":""},
                {"title":"B","content":"B","applies_when":"   "},
                {"title":"C","content":"C","applies_when":"\n\t"}
            ],
            "patterns": [],
            "pitfalls": []
        }"#;
        let result = parse_extraction_response(json).expect("empty anchor json parses");
        for note in &result.cases {
            assert!(
                note.normalized_anchor().is_none(),
                "empty/whitespace anchor must normalize to None; got {:?}",
                note.normalized_anchor()
            );
        }
    }
    #[test]
    fn normalized_anchor_trims_surrounding_whitespace() {
        let note = ExtractedNote {
            title: "T".to_string(),
            content: "C".to_string(),
            retrieval_anchor: Some("  When trimming matters.  \n".to_string()),
            scope_paths: vec![],
        };
        assert_eq!(
            note.normalized_anchor().as_deref(),
            Some("When trimming matters.")
        );
    }
    #[test]
    fn terminal_prompt_distinguishes_completed_review_rejection_and_ci_failure() {
        let completed = build_extraction_prompt(
            "title",
            "desc",
            "{}",
            "transcript",
            "[]",
            &TerminalExtractionContext {
                outcome: TerminalExtractionOutcome::Completed,
                review_decision: None,
            },
        );
        let review_rejected = build_extraction_prompt(
            "title",
            "desc",
            "{}",
            "transcript",
            "[]",
            &TerminalExtractionContext {
                outcome: TerminalExtractionOutcome::Parked {
                    classification: "acceptance_criteria".to_string(),
                    reason: Some("required evidence missing".to_string()),
                },
                review_decision: Some(TerminalReviewDecision::Rejected {
                    reason: Some("acceptance criteria rejected".to_string()),
                }),
            },
        );
        let ci_failed = build_extraction_prompt(
            "title",
            "desc",
            "{}",
            "transcript",
            "[]",
            &TerminalExtractionContext {
                outcome: TerminalExtractionOutcome::Parked {
                    classification: "ci_failure".to_string(),
                    reason: Some("Quality Gate failed".to_string()),
                },
                review_decision: None,
            },
        );

        assert!(completed.contains("Outcome: completed successfully"));
        assert!(completed.contains("no explicit review decision recorded; do not infer one"));
        assert!(review_rejected.contains("classification: acceptance_criteria"));
        assert!(review_rejected.contains("Explicit review decision: rejected"));
        assert!(ci_failed.contains("classification: ci_failure"));
        assert!(!ci_failed.contains("Explicit review decision: rejected"));
        assert!(review_rejected.contains("failed approach or pitfall"));
        assert!(ci_failed.contains("do not frame it as a neutral or successful pattern"));
    }

    #[test]
    fn terminal_context_serializes_unknown_without_a_fabricated_verdict() {
        let context = TerminalExtractionContext::unknown_historical();
        assert_eq!(context.review_decision, None);
        assert_eq!(
            serde_json::to_value(context).expect("terminal context serializes"),
            serde_json::json!({"outcome": {"kind": "unknown_historical"}})
        );
    }

    #[test]
    fn prompt_template_requires_applies_when_field() {
        // The full extraction prompt must explicitly request `applies_when`
        // and the ADR-054 sections. (Reuse the production prompt builder so
        // the test cannot drift from the live template.)
        let prompt = build_extraction_prompt(
            "title-x",
            "desc-y",
            "{}",
            "(none)",
            "[]",
            &TerminalExtractionContext::unknown_historical(),
        );
        assert!(
            prompt.contains("\"applies_when\""),
            "prompt must include the applies_when field name"
        );
        // All three ADR-054 section lists must remain — the new anchor field
        // must not displace the durable body schema.
        assert!(prompt.contains("## Recommended approach"));
        assert!(prompt.contains("## Failure mode"));
        assert!(prompt.contains("## Approach taken"));
        // Anchor must be required to be distinct from the body.
        assert!(prompt.contains("DISTINCT from the markdown body"));
        assert!(prompt.contains("\"revision_operations\""));
        assert!(prompt.contains("deprecate_with_supersedes"));
        assert!(prompt.contains("IDs are proposals only"));
    }
    #[test]
    fn extraction_quality_defaults_to_zero() {
        assert_eq!(ExtractionQuality::default().novelty_skipped, 0);
    }
    #[test]
    fn extraction_max_tokens_is_raised_value() {
        // Guards against a regression back to the truncating 1024 cap.
        assert_eq!(EXTRACTION_MAX_TOKENS, 4096);
    }
    #[test]
    fn dedup_extracted_notes_collapses_same_normalized_title_and_type() {
        // Two cases with the same title modulo case/whitespace must collapse to one.
        let extracted = ExtractionResponse {
            cases: vec![
                ExtractedNote {
                    title: "Flaky Extraction".to_string(),
                    content: "first body".to_string(),
                    retrieval_anchor: Some("When extraction is flaky across runs.".to_string()),
                    scope_paths: vec![],
                },
                ExtractedNote {
                    title: "  flaky extraction ".to_string(),
                    content: "second body (duplicate)".to_string(),
                    retrieval_anchor: Some("When extraction is flaky across runs.".to_string()),
                    scope_paths: vec![],
                },
            ],
            patterns: vec![],
            pitfalls: vec![],
            revision_operations: vec![],
        };
        let (deduped, dupes) = dedup_extracted_notes(&extracted);
        assert_eq!(dupes, 1, "one intra-batch duplicate should be dropped");
        assert_eq!(deduped.len(), 1, "only the first-seen note is kept");
        assert_eq!(deduped[0].0, "case");
        assert_eq!(deduped[0].1.content, "first body");
    }
    #[test]
    fn dedup_extracted_notes_keeps_same_title_across_different_types() {
        // Same title but different note_type are NOT duplicates (key includes type).
        let extracted = ExtractionResponse {
            cases: vec![ExtractedNote {
                title: "Shared Title".to_string(),
                content: "case body".to_string(),
                retrieval_anchor: None,
                scope_paths: vec![],
            }],
            patterns: vec![],
            pitfalls: vec![ExtractedNote {
                title: "shared title".to_string(),
                content: "pitfall body".to_string(),
                retrieval_anchor: None,
                scope_paths: vec![],
            }],
            revision_operations: vec![],
        };
        let (deduped, dupes) = dedup_extracted_notes(&extracted);
        assert_eq!(dupes, 0);
        assert_eq!(deduped.len(), 2);
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
        let _ = test_path(&format!("djinn-llm-extraction-no-task-{id}-"));
        let name = format!("proj-{id}");
        let project = project_repo
            .create(&name, "test", &name)
            .await
            .expect("create project");
        let session = session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &project.id,
                task_id: None, // no task_id
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
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
        let _ = test_path(&format!("djinn-llm-extraction-provider-{id}-"));
        let name = format!("proj-{id}");
        let project = project_repo
            .create(&name, "test", &name)
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
                    blocked_by: None,
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
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
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

    #[test]
    fn novelty_prompt_renders_existing_full_body_and_incoming_extraction() {
        let note = ExtractedNote {
            title: "Proposed Note Title".to_string(),
            content: "Proposed note body.".to_string(),
            retrieval_anchor: None,
            scope_paths: vec![],
        };
        let candidate = NoteDedupCandidate {
            id: "candidate-1".to_string(),
            permalink: "cases/candidate-1".to_string(),
            title: "Existing Candidate".to_string(),
            folder: "cases".to_string(),
            note_type: "case".to_string(),
            content: "Full existing candidate body from the database.".to_string(),
            abstract_: Some("Abstract summary for the candidate.".to_string()),
            overview: Some("Overview summary for the candidate.".to_string()),
            score: 0.95,
        };
        let prompt = build_novelty_prompt("case", &note, "Incoming proposed summary", &[candidate]);
        assert!(
            prompt.contains("Incoming proposed summary"),
            "incoming extraction summary must be visible"
        );
        assert!(
            prompt.contains("Full existing candidate body from the database."),
            "existing candidate body must be visible"
        );
        assert!(
            prompt.contains("Abstract summary for the candidate."),
            "abstract/overview may remain as supplemental metadata"
        );
        assert!(
            !prompt.contains("truncated"),
            "below-cap body must not have a truncation marker"
        );
    }

    #[test]
    fn novelty_prompt_keeps_body_at_cap_without_truncation_marker() {
        let note = ExtractedNote {
            title: "T".to_string(),
            content: "C".to_string(),
            retrieval_anchor: None,
            scope_paths: vec![],
        };
        let body = "a".repeat(NOVELTY_CANDIDATE_CONTENT_CHAR_CAP);
        let candidate = NoteDedupCandidate {
            id: "c".to_string(),
            permalink: "cases/c".to_string(),
            title: "Candidate".to_string(),
            folder: "cases".to_string(),
            note_type: "case".to_string(),
            content: body.clone(),
            abstract_: None,
            overview: None,
            score: 1.0,
        };
        let prompt = build_novelty_prompt("case", &note, "summary", &[candidate]);
        assert!(
            prompt.contains(&body),
            "body exactly at cap must be fully present"
        );
        assert!(
            !prompt.contains("truncated"),
            "exactly-at-cap body must not have a truncation marker"
        );
    }

    #[test]
    fn novelty_prompt_truncates_body_above_cap() {
        let note = ExtractedNote {
            title: "T".to_string(),
            content: "C".to_string(),
            retrieval_anchor: None,
            scope_paths: vec![],
        };
        let body = "a".repeat(NOVELTY_CANDIDATE_CONTENT_CHAR_CAP + 50);
        let candidate = NoteDedupCandidate {
            id: "c".to_string(),
            permalink: "cases/c".to_string(),
            title: "Candidate".to_string(),
            folder: "cases".to_string(),
            note_type: "case".to_string(),
            content: body,
            abstract_: None,
            overview: None,
            score: 1.0,
        };
        let prompt = build_novelty_prompt("case", &note, "summary", &[candidate]);
        let expected_present = "a".repeat(NOVELTY_CANDIDATE_CONTENT_CHAR_CAP);
        let expected_absent = "a".repeat(NOVELTY_CANDIDATE_CONTENT_CHAR_CAP + 1);
        assert!(
            prompt.contains(&expected_present),
            "capped body must include up to the cap"
        );
        assert!(
            !prompt.contains(&expected_absent),
            "content beyond the cap must be absent"
        );
        assert!(
            prompt.contains("… [truncated"),
            "above-cap body must have a truncation marker"
        );
    }

    #[test]
    fn novelty_prompt_truncation_respects_unicode_boundaries() {
        let note = ExtractedNote {
            title: "T".to_string(),
            content: "C".to_string(),
            retrieval_anchor: None,
            scope_paths: vec![],
        };
        let prefix = "a".repeat(NOVELTY_CANDIDATE_CONTENT_CHAR_CAP - 1);
        let suffix = "🙂 beyond-cap-marker";
        let body = format!("{prefix}{suffix}");
        let candidate = NoteDedupCandidate {
            id: "c".to_string(),
            permalink: "cases/c".to_string(),
            title: "Candidate".to_string(),
            folder: "cases".to_string(),
            note_type: "case".to_string(),
            content: body,
            abstract_: None,
            overview: None,
            score: 1.0,
        };
        let prompt = build_novelty_prompt("case", &note, "summary", &[candidate]);
        let expected_present = format!("{prefix}🙂");
        assert!(
            prompt.contains(&expected_present),
            "multi-byte character must be kept whole"
        );
        assert!(
            !prompt.contains("beyond-cap-marker"),
            "content beyond the cap must be absent"
        );
        assert!(
            prompt.contains("… [truncated"),
            "truncation must be signaled"
        );
    }
}

#[cfg(test)]
mod evidence_merge_contract_tests {
    use super::*;

    #[test]
    fn evidence_merge_keeps_existing_provenance_footer_exactly_once() {
        let existing = "existing evidence\n\n---\n*Extracted from session old. Confidence: 0.5 (session-extracted).*";
        let model = "merged evidence\n\n---\n*Extracted from session old. Confidence: 0.5 (session-extracted).*\n\n---\n*Extracted from session old. Confidence: 0.5 (session-extracted).*";
        let content = content_with_one_provenance_footer(model, existing, "fallback");
        assert_eq!(
            content,
            "merged evidence\n\n---\n*Extracted from session old. Confidence: 0.5 (session-extracted).*"
        );
    }
}

#[cfg(test)]
mod evidence_merge_regression_tests {
    use super::*;
    use crate::session_extraction::ExtractionQuality;
    use djinn_core::message::ContentBlock;
    use djinn_db::NoteDedupCandidate;
    use djinn_provider::message::Conversation;
    use djinn_provider::provider::{StreamEvent, ToolChoice};
    use futures::{Future, Stream};
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    enum RepoOp {
        Get(String),
        Update {
            id: String,
            title: String,
            content: String,
            tags: String,
        },
        UpdateConfidence {
            id: String,
            signal: f64,
        },
    }

    #[derive(Debug, Clone)]
    struct RevisionRecord {
        mutation: NoteRevisionMutation,
        before_content: Option<String>,
        before_confidence: Option<f64>,
        after_content: Option<String>,
        after_confidence: Option<f64>,
        changed: bool,
        committed_note_id: Option<String>,
        revision_id: Option<String>,
    }

    struct RecordingExtractionRepository {
        ops: Arc<Mutex<Vec<RepoOp>>>,
        revisions: Arc<Mutex<Vec<RevisionRecord>>>,
        existing: Arc<Mutex<Option<djinn_memory::Note>>>,
        /// Fail one canonical mutation before its fake state or ledger record
        /// changes, while still allowing the later ExtractionSkipped mutation.
        fail_next_kind: Arc<Mutex<Option<NoteRevisionEventKind>>>,
    }

    impl RecordingExtractionRepository {
        fn empty() -> Self {
            Self {
                ops: Arc::new(Mutex::new(Vec::new())),
                revisions: Arc::new(Mutex::new(Vec::new())),
                existing: Arc::new(Mutex::new(None)),
                fail_next_kind: Arc::new(Mutex::new(None)),
            }
        }

        fn empty_with_mutation_failure(event_kind: NoteRevisionEventKind) -> Self {
            let repo = Self::empty();
            *repo.fail_next_kind.lock().unwrap() = Some(event_kind);
            repo
        }

        fn with_existing(existing: djinn_memory::Note) -> Self {
            Self {
                ops: Arc::new(Mutex::new(Vec::new())),
                revisions: Arc::new(Mutex::new(Vec::new())),
                existing: Arc::new(Mutex::new(Some(existing))),
                fail_next_kind: Arc::new(Mutex::new(None)),
            }
        }

        fn with_mutation_failure(
            existing: djinn_memory::Note,
            event_kind: NoteRevisionEventKind,
        ) -> Self {
            Self {
                ops: Arc::new(Mutex::new(Vec::new())),
                revisions: Arc::new(Mutex::new(Vec::new())),
                existing: Arc::new(Mutex::new(Some(existing))),
                fail_next_kind: Arc::new(Mutex::new(Some(event_kind))),
            }
        }

        fn ops(&self) -> Vec<RepoOp> {
            self.ops.lock().unwrap().clone()
        }

        fn revisions(&self) -> Vec<RevisionRecord> {
            self.revisions.lock().unwrap().clone()
        }

        fn clear_revisions(&self) {
            self.revisions.lock().unwrap().clear();
        }

        fn existing_content(&self) -> String {
            self.existing
                .lock()
                .unwrap()
                .as_ref()
                .expect("test repository retains existing note")
                .content
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl ExtractionNoteRepository for RecordingExtractionRepository {
        async fn mutate_with_revision(
            &self,
            mutation: NoteRevisionMutation,
        ) -> djinn_db::Result<djinn_db::NoteRevisionMutationResult> {
            if self
                .fail_next_kind
                .lock()
                .unwrap()
                .take_if(|kind| *kind == mutation.event_kind)
                .is_some()
            {
                return Err(djinn_db::Error::Internal(
                    "controlled revision mutation failure".to_owned(),
                ));
            }
            let mut existing = self.existing.lock().unwrap();
            let (before_content, before_confidence, changed, committed_note) =
                match &mutation.desired {
                    NoteRevisionDesiredState::Create(desired) => {
                        let note = djinn_memory::Note {
                            id: mutation.note_id.clone().expect("create has note id"),
                            project_id: mutation.project_id.clone(),
                            permalink: desired.permalink.clone(),
                            title: desired.title.clone(),
                            file_path: String::new(),
                            storage: "db".to_owned(),
                            note_type: desired.note_type.clone(),
                            folder: desired.folder.clone(),
                            status: desired.status.clone(),
                            tags: desired.tags.clone(),
                            content: desired.content.clone(),
                            retrieval_anchor: desired.retrieval_anchor.clone(),
                            created_at: "2026-01-01T00:00:00Z".to_owned(),
                            updated_at: "2026-01-01T00:00:00Z".to_owned(),
                            last_accessed: "2026-01-01T00:00:00Z".to_owned(),
                            access_count: 0,
                            confidence: desired.confidence,
                            abstract_: None,
                            overview: None,
                            scope_paths: desired.scope_paths.clone(),
                        };
                        *existing = Some(note.clone());
                        (None, None, true, Some(note))
                    }
                    NoteRevisionDesiredState::Existing {
                        content,
                        confidence,
                    } => {
                        let note = existing.as_mut().ok_or_else(|| {
                            djinn_db::Error::Internal("missing test note".to_owned())
                        })?;
                        let before_content = note.content.clone();
                        let before_confidence = note.confidence;
                        let changed = note.content != *content || note.confidence != *confidence;
                        if changed {
                            note.content.clone_from(content);
                            note.confidence = *confidence;
                        }
                        (
                            Some(before_content),
                            Some(before_confidence),
                            changed,
                            Some(note.clone()),
                        )
                    }
                    NoteRevisionDesiredState::ExistingWithMetadata(_) => {
                        unreachable!("extraction never submits metadata updates")
                    }
                    NoteRevisionDesiredState::GuardedPatch { .. }
                    | NoteRevisionDesiredState::DeprecateWithSupersedes { .. } => {
                        unreachable!(
                            "this legacy extraction test seam never submits revision operations"
                        )
                    }
                    NoteRevisionDesiredState::ExtractionSkipped => (None, None, true, None),
                    NoteRevisionDesiredState::Delete => unreachable!("not used by extraction"),
                };
            let revision_id = changed.then(|| "test-revision".to_owned());
            self.revisions.lock().unwrap().push(RevisionRecord {
                mutation,
                before_content,
                before_confidence,
                after_content: committed_note.as_ref().map(|note| note.content.clone()),
                after_confidence: committed_note.as_ref().map(|note| note.confidence),
                changed,
                committed_note_id: committed_note.as_ref().map(|note| note.id.clone()),
                revision_id: revision_id.clone(),
            });
            Ok(djinn_db::NoteRevisionMutationResult {
                changed,
                note: committed_note,
                note_seq: changed.then_some(1),
                revision_id,
                deprecated_note_id: None,
                superseding_note_id: None,
                supersedes_association: None,
            })
        }
        async fn get(&self, id: &str) -> djinn_db::Result<Option<djinn_memory::Note>> {
            self.ops.lock().unwrap().push(RepoOp::Get(id.to_string()));
            Ok(self.existing.lock().unwrap().clone())
        }
        async fn update(
            &self,
            id: &str,
            title: &str,
            content: &str,
            tags: &str,
        ) -> djinn_db::Result<djinn_memory::Note> {
            self.ops.lock().unwrap().push(RepoOp::Update {
                id: id.to_string(),
                title: title.to_string(),
                content: content.to_string(),
                tags: tags.to_string(),
            });
            let mut note = self.existing.lock().unwrap().clone().unwrap();
            note.title = title.to_string();
            note.content = content.to_string();
            note.tags = tags.to_string();
            Ok(note)
        }
        async fn update_confidence(&self, note_id: &str, signal: f64) -> djinn_db::Result<f64> {
            self.ops.lock().unwrap().push(RepoOp::UpdateConfidence {
                id: note_id.to_string(),
                signal,
            });
            Ok(signal)
        }
        async fn set_confidence(&self, _note_id: &str, _value: f64) -> djinn_db::Result<()> {
            Ok(())
        }
        async fn get_by_permalink(
            &self,
            _project_id: &str,
            permalink: &str,
        ) -> djinn_db::Result<Option<djinn_memory::Note>> {
            Ok(self
                .existing
                .lock()
                .unwrap()
                .as_ref()
                .filter(|note| note.permalink == permalink)
                .cloned())
        }
        async fn create_db_note_with_scope_and_retrieval_anchor(
            &self,
            _project_id: &str,
            _title: &str,
            _content: &str,
            _note_type: &str,
            _tags: &str,
            _scope_paths: &str,
            _retrieval_anchor: Option<&str>,
        ) -> djinn_db::Result<djinn_memory::Note> {
            unimplemented!("not used in this regression")
        }
        async fn create_with_scope_and_retrieval_anchor(
            &self,
            _project_id: &str,
            _title: &str,
            _content: &str,
            _note_type: &str,
            _permalink: Option<&str>,
            _tags: &str,
            _scope_paths: &str,
            _retrieval_anchor: Option<&str>,
        ) -> djinn_db::Result<djinn_memory::Note> {
            unimplemented!("not used in this regression")
        }
        async fn create_with_scope(
            &self,
            _project_id: &str,
            _title: &str,
            _content: &str,
            _note_type: &str,
            _permalink: Option<&str>,
            _tags: &str,
            _scope_paths: &str,
        ) -> djinn_db::Result<djinn_memory::Note> {
            unimplemented!("not used in this regression")
        }
        async fn update_scope_paths(
            &self,
            _id: &str,
            _scope_paths: &str,
        ) -> djinn_db::Result<djinn_memory::Note> {
            unimplemented!("not used in this regression")
        }
        async fn dedup_candidates(
            &self,
            _project_id: &str,
            _folder: &str,
            _note_type: &str,
            _text: &str,
            _limit: usize,
        ) -> djinn_db::Result<Vec<djinn_db::NoteDedupCandidate>> {
            Ok(Vec::new())
        }
    }

    enum ScriptedProviderResponse {
        Text(String),
        TransportError,
    }

    struct ScriptedProvider {
        responses: Mutex<VecDeque<ScriptedProviderResponse>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(ScriptedProviderResponse::Text)
                        .collect(),
                ),
            }
        }

        fn with_transport_error_after(responses: Vec<String>) -> Self {
            let mut responses: VecDeque<_> = responses
                .into_iter()
                .map(ScriptedProviderResponse::Text)
                .collect();
            responses.push_back(ScriptedProviderResponse::TransportError);
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl LlmProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [serde_json::Value],
            _tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = anyhow::Result<
                            Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ScriptedProviderResponse::Text(String::new()));
            if matches!(response, ScriptedProviderResponse::TransportError) {
                return Box::pin(async {
                    Err(anyhow::anyhow!("controlled provider transport failure"))
                });
            }
            let ScriptedProviderResponse::Text(response) = response else {
                unreachable!("transport errors return before constructing a stream");
            };
            let stream = futures::stream::iter(vec![
                Ok(StreamEvent::Delta(ContentBlock::Text { text: response })),
                Ok(StreamEvent::Done),
            ]);
            Box::pin(async move { Ok(Box::pin(stream) as _) })
        }
    }

    fn test_existing_note() -> djinn_memory::Note {
        let footer = "---\n*Extracted from session old. Confidence: 0.5 (session-extracted).*";
        djinn_memory::Note {
            id: "existing-note-1".to_string(),
            project_id: "project-1".to_string(),
            permalink: "cases/existing-note-1".to_string(),
            title: "Existing Case".to_string(),
            file_path: String::new(),
            storage: "db".to_string(),
            note_type: "case".to_string(),
            folder: "cases".to_string(),
            status: "active".to_string(),
            tags: "[]".to_string(),
            content: format!("Existing evidence\n\n{footer}"),
            retrieval_anchor: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed: "2026-01-01T00:00:00Z".to_string(),
            access_count: 0,
            confidence: 0.5,
            abstract_: None,
            overview: None,
            scope_paths: "[]".to_string(),
        }
    }

    fn test_extracted_case() -> ExtractedNote {
        ExtractedNote {
            title: "Fresh Case".to_string(),
            content: "## Situation\nA session-extracted case note is produced during task finalization.\n\n## Constraint\nThe existing candidate must be a low-confidence, session-extracted note with a provenance footer.\n\n## Approach taken\nMerge the fresh evidence into the existing note body and keep the provenance footer.\n\n## Result\nThe merged body contains both the original evidence and the new evidence, with the footer preserved.\n\n## Why it worked / failed\nThe merge preserves provenance and avoids wholesale replacement while allowing future sessions to contribute additional evidence.\n\n## Reusable lesson\nSession-extracted notes below the curation threshold can absorb new evidence from later sessions without losing their original provenance.\n\n## Related\n- extraction merge\n- provenance\n".to_string(),
            retrieval_anchor: None,
            scope_paths: vec![],
        }
    }

    fn test_candidate() -> NoteDedupCandidate {
        NoteDedupCandidate {
            id: "existing-note-1".to_string(),
            permalink: "cases/existing-note-1".to_string(),
            title: "Existing Case".to_string(),
            folder: "cases".to_string(),
            note_type: "case".to_string(),
            content: "Existing evidence\n\n---\n*Extracted from session old. Confidence: 0.5 (session-extracted).*".to_string(),
            abstract_: None,
            overview: None,
            score: 1.0,
        }
    }

    fn test_candidate_lookup(
        _project_id: &str,
        _folder: &str,
        _note_type: &str,
        _abstract: &str,
    ) -> Vec<NoteDedupCandidate> {
        vec![test_candidate()]
    }

    fn test_context<'a>(
        repo: &'a dyn ExtractionNoteRepository,
        provider: &'a dyn LlmProvider,
    ) -> ExtractionContext<'a> {
        ExtractionContext {
            note_repo: repo,
            provider,
            project_id: "project-1",
            project_path: "/projects/project-1",
            knowledge_branch_target: &KnowledgeBranchTarget::Main,
            session_id: "new",
            task_id: "task-1",
            task_run_id: Some("run-1"),
            task_short_id: "t1",
            task_title: "Test task",
            task_description: "Test task description",
            provenance: "\n\n---\n*Extracted from session new. Confidence: 0.5 (session-extracted).*",
            caller_attributed: true,
            session_scope_paths: &[],
            candidate_lookup: CandidateLookup::with_override(test_candidate_lookup),
        }
    }

    async fn finalize_test_output(
        repo: &dyn ExtractionNoteRepository,
        durable_output_count: usize,
    ) {
        finalize_extraction_output(
            repo,
            "project-1",
            "new",
            "task-1",
            Some("run-1"),
            durable_output_count,
        )
        .await;
    }

    fn assert_extraction_identity(record: &RevisionRecord, reason: &str) {
        assert_eq!(
            record.mutation.attribution,
            TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction)
        );
        assert_eq!(record.mutation.provenance.session_id(), Some("new"));
        assert_eq!(record.mutation.provenance.task_id(), Some("task-1"));
        assert_eq!(record.mutation.provenance.task_run_id(), Some("run-1"));
        assert_eq!(record.mutation.reason.as_str(), reason);
        assert!(record.changed);
        assert!(record.revision_id.is_some());
    }

    #[tokio::test]
    async fn atomic_extracted_note_create_records_exact_revision_contract() {
        let provider = ScriptedProvider::new(vec![]);
        let repo = RecordingExtractionRepository::empty();
        let context = ExtractionContext {
            note_repo: &repo,
            provider: &provider,
            project_id: "project-1",
            project_path: "/projects/project-1",
            knowledge_branch_target: &KnowledgeBranchTarget::Main,
            session_id: "new",
            task_id: "task-1",
            task_run_id: Some("run-1"),
            task_short_id: "t1",
            task_title: "Test task",
            task_description: "Test task description",
            provenance: "footer",
            caller_attributed: true,
            session_scope_paths: &[],
            candidate_lookup: CandidateLookup::with_override(test_candidate_lookup),
        };
        let result = context
            .create_extracted_note(
                "Created extraction",
                "created body",
                "case",
                r#"["src/lib.rs"]"#,
                Some("created retrieval anchor"),
            )
            .await
            .expect("atomic extraction create succeeds");
        assert_eq!(usize::from(result.changed), 1);

        let revisions = repo.revisions();
        assert_eq!(revisions.len(), 1);
        let created = &revisions[0];
        assert_eq!(created.mutation.event_kind, NoteRevisionEventKind::Created);
        assert_extraction_identity(created, "created note from completed session extraction");
        assert_eq!(created.before_content, None);
        assert_eq!(created.before_confidence, None);
        assert_eq!(created.after_content.as_deref(), Some("created body"));
        assert_eq!(created.after_confidence, Some(0.5));
        assert!(created.committed_note_id.is_some());
        assert!(
            result
                .note
                .as_ref()
                .is_some_and(|note| note.confidence == 0.5)
        );
        assert!(result.revision_id.is_some());
    }

    #[tokio::test]
    async fn extraction_skipped_records_exact_revision_contract() {
        let repo = RecordingExtractionRepository::empty();
        record_extraction_skipped(&repo, "project-1", "new", "task-1", Some("run-1")).await;

        let revisions = repo.revisions();
        assert_eq!(revisions.len(), 1);
        let skipped = &revisions[0];
        assert_eq!(
            skipped.mutation.event_kind,
            NoteRevisionEventKind::ExtractionSkipped
        );
        assert_eq!(skipped.mutation.note_id, None);
        assert_eq!(
            skipped.mutation.desired,
            NoteRevisionDesiredState::ExtractionSkipped
        );
        assert_extraction_identity(skipped, EXTRACTION_SKIPPED_REASON);
        assert_eq!(skipped.before_content, None);
        assert_eq!(skipped.before_confidence, None);
        assert_eq!(skipped.after_content, None);
        assert_eq!(skipped.after_confidence, None);
        assert_eq!(skipped.committed_note_id, None);
    }

    #[tokio::test]
    async fn terminal_completion_transport_parse_and_empty_paths_record_one_skipped_revision() {
        // The outer runner maps each of these paths to this shared terminal
        // boundary. Keep the fake-backed assertion here so a future branch
        // cannot bypass the canonical skipped mutation or its provenance.
        for terminal_path in [
            "completion transport",
            "timeout",
            "parse failure",
            "parsed empty",
        ] {
            let repo = RecordingExtractionRepository::empty();
            finalize_test_output(&repo, 0).await;

            let revisions = repo.revisions();
            assert_eq!(
                revisions.len(),
                1,
                "{terminal_path} must record one terminal revision"
            );
            assert_eq!(
                revisions[0].mutation.event_kind,
                NoteRevisionEventKind::ExtractionSkipped,
                "{terminal_path} must record extraction skipped"
            );
            assert_extraction_identity(&revisions[0], EXTRACTION_SKIPPED_REASON);
        }
    }

    #[tokio::test]
    async fn terminal_admission_drop_records_one_trusted_skipped_revision() {
        let provider = ScriptedProvider::new(vec![]);
        let repo = RecordingExtractionRepository::empty();
        let context = test_context(&repo, &provider);
        let dropped = ExtractedNote {
            title: "Incomplete".to_owned(),
            content: "too short for durable memory".to_owned(),
            retrieval_anchor: None,
            scope_paths: vec![],
        };
        let mut quality = ExtractionQuality::default();
        let durable = process_extracted_note(&context, "case", &dropped, &mut quality).await;
        assert_eq!(durable, 0);
        assert_eq!(quality.admission_dropped, 1);

        finalize_test_output(&repo, durable).await;
        let revisions = repo.revisions();
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            revisions[0].mutation.event_kind,
            NoteRevisionEventKind::ExtractionSkipped
        );
        assert_extraction_identity(&revisions[0], EXTRACTION_SKIPPED_REASON);
    }

    #[tokio::test]
    async fn terminal_duplicate_confidence_noop_records_one_trusted_skipped_revision() {
        let provider = ScriptedProvider::new(vec![
            r#"{"decision":"already_known","existing_note_id":"existing-note-1"}"#.to_owned(),
        ]);
        let mut existing = test_existing_note();
        existing.confidence = 1.0;
        let repo = RecordingExtractionRepository::with_existing(existing);
        let context = test_context(&repo, &provider);
        let mut quality = ExtractionQuality::default();
        let durable =
            process_extracted_note(&context, "case", &test_extracted_case(), &mut quality).await;
        assert_eq!(durable, 0);

        finalize_test_output(&repo, durable).await;
        let revisions = repo.revisions();
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            revisions[0].mutation.event_kind,
            NoteRevisionEventKind::ExtractionSkipped
        );
        assert_extraction_identity(&revisions[0], EXTRACTION_SKIPPED_REASON);
    }

    #[tokio::test]
    async fn terminal_unchanged_working_spec_records_one_trusted_skipped_revision() {
        let provider = ScriptedProvider::new(vec![]);
        let setup_repo = RecordingExtractionRepository::empty();
        let setup_context = test_context(&setup_repo, &provider);
        let section = render_working_spec_entry(
            &setup_context,
            &test_extracted_case(),
            &["test reason"],
            &[],
        );
        let mut existing = test_existing_note();
        existing.permalink = permalink_for("design", "Working Spec t1");
        existing.content = render_working_spec_document(&setup_context, &section, &[]);
        let repo = RecordingExtractionRepository::with_existing(existing);
        let context = test_context(&repo, &provider);

        let changed =
            persist_working_spec(&context, &test_extracted_case(), &["test reason"]).await;
        assert!(!changed);
        finalize_test_output(&repo, usize::from(changed)).await;
        let revisions = repo.revisions();
        assert_eq!(
            revisions.len(),
            2,
            "the no-op mutation and terminal skip are recorded"
        );
        assert!(!revisions[0].changed);
        assert_eq!(
            revisions[0].mutation.event_kind,
            NoteRevisionEventKind::Updated
        );
        assert_eq!(
            revisions[1].mutation.event_kind,
            NoteRevisionEventKind::ExtractionSkipped
        );
        assert_extraction_identity(&revisions[1], EXTRACTION_SKIPPED_REASON);
    }

    #[tokio::test]
    async fn terminal_mutation_failure_does_not_fabricate_output_and_records_skip() {
        let provider = ScriptedProvider::new(vec![r#"{"decision":"novel"}"#.to_owned()]);
        let repo = RecordingExtractionRepository::empty_with_mutation_failure(
            NoteRevisionEventKind::Created,
        );
        let context = test_context(&repo, &provider);
        let mut quality = ExtractionQuality::default();
        let durable =
            process_extracted_note(&context, "case", &test_extracted_case(), &mut quality).await;
        assert_eq!(durable, 0);

        finalize_test_output(&repo, durable).await;
        let revisions = repo.revisions();
        assert_eq!(
            revisions.len(),
            1,
            "failed create must not fabricate a revision"
        );
        assert_eq!(
            revisions[0].mutation.event_kind,
            NoteRevisionEventKind::ExtractionSkipped
        );
        assert_extraction_identity(&revisions[0], EXTRACTION_SKIPPED_REASON);
    }

    #[tokio::test]
    async fn terminal_durable_success_suppresses_skipped_revision() {
        let provider = ScriptedProvider::new(vec![]);
        let repo = RecordingExtractionRepository::empty();
        let context = test_context(&repo, &provider);
        let created = context
            .create_extracted_note("Partial success", "durable body", "case", "[]", None)
            .await
            .expect("create succeeds");
        finalize_test_output(&repo, usize::from(created.changed)).await;

        let revisions = repo.revisions();
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            revisions[0].mutation.event_kind,
            NoteRevisionEventKind::Created
        );
        assert_extraction_identity(
            &revisions[0],
            "created note from completed session extraction",
        );
    }

    #[tokio::test]
    async fn evidence_merge_persists_content_before_confidence() {
        let provider = ScriptedProvider::new(vec![
            r#"{"decision":"already_known","existing_note_id":"existing-note-1"}"#.to_string(),
            r#"{"content":"Merged evidence\n\nThe merged note now includes fresh evidence from the latest session and the original evidence."}"#.to_string(),
        ]);
        let repo = RecordingExtractionRepository::with_existing(test_existing_note());
        let provenance =
            "\n\n---\n*Extracted from session new. Confidence: 0.5 (session-extracted).*";
        let context = ExtractionContext {
            note_repo: &repo,
            provider: &provider,
            project_id: "project-1",
            project_path: "/projects/project-1",
            knowledge_branch_target: &KnowledgeBranchTarget::Main,
            session_id: "new",
            task_id: "task-1",
            task_run_id: Some("run-1"),
            task_short_id: "t1",
            task_title: "Test task",
            task_description: "Test task description",
            provenance,
            caller_attributed: true,
            session_scope_paths: &[],
            candidate_lookup: CandidateLookup::with_override(test_candidate_lookup),
        };
        let mut quality = ExtractionQuality::default();
        process_extracted_note(&context, "case", &test_extracted_case(), &mut quality).await;

        let revisions = repo.revisions();
        let update_pos = revisions
            .iter()
            .position(|op| op.mutation.event_kind == NoteRevisionEventKind::Updated);
        let confidence_pos = revisions
            .iter()
            .position(|op| op.mutation.event_kind == NoteRevisionEventKind::ConfidenceChanged);
        assert!(update_pos.is_some(), "content update must be recorded");
        assert!(
            confidence_pos.is_some(),
            "confidence update must be recorded"
        );
        assert!(
            update_pos.unwrap() < confidence_pos.unwrap(),
            "content update must complete before confidence update"
        );

        let updated_content = revisions
            .iter()
            .find(|revision| revision.mutation.event_kind == NoteRevisionEventKind::Updated)
            .and_then(|revision| revision.after_content.clone())
            .expect("update op must include content");
        assert!(
            updated_content
                .contains("The merged note now includes fresh evidence from the latest session"),
            "merged content must contain the fresh evidence"
        );
        let footer = "---\n*Extracted from session old. Confidence: 0.5 (session-extracted).*";
        assert_eq!(
            updated_content.matches(footer).count(),
            1,
            "the existing provenance footer must be preserved exactly once"
        );
        assert_eq!(quality.evidence_merged, 1);
        assert_eq!(quality.boost_fallback, 0);

        let updated = &revisions[update_pos.unwrap()];
        assert_extraction_identity(updated, "merged extracted evidence into existing note");
        assert_eq!(
            updated.before_content.as_deref(),
            Some(test_existing_note().content.as_str())
        );
        assert_eq!(updated.before_confidence, Some(0.5));
        assert_eq!(
            updated.after_content.as_deref(),
            Some(updated_content.as_str())
        );
        assert_eq!(updated.after_confidence, Some(0.5));
        assert!(updated.committed_note_id.is_some());

        let confidence = &revisions[confidence_pos.unwrap()];
        assert_extraction_identity(confidence, "confirmed duplicate extracted knowledge");
        assert_eq!(
            confidence.before_content.as_deref(),
            Some(updated_content.as_str())
        );
        assert_eq!(
            confidence.after_content.as_deref(),
            Some(updated_content.as_str())
        );
        assert_eq!(confidence.before_confidence, Some(0.5));
        assert_eq!(confidence.after_confidence, Some(0.65));
        assert!(confidence.committed_note_id.is_some());
    }

    #[tokio::test]
    async fn provider_transport_failure_falls_back_without_aborting_extraction() {
        let provider = ScriptedProvider::with_transport_error_after(vec![
            r#"{"decision":"already_known","existing_note_id":"existing-note-1"}"#.to_string(),
        ]);
        let repo = RecordingExtractionRepository::with_existing(test_existing_note());
        let context = ExtractionContext {
            note_repo: &repo,
            provider: &provider,
            project_id: "project-1",
            project_path: "/projects/project-1",
            knowledge_branch_target: &KnowledgeBranchTarget::Main,
            session_id: "new",
            task_id: "task-1",
            task_run_id: None,
            task_short_id: "t1",
            task_title: "Test task",
            task_description: "Test task description",
            provenance: "\n\n---\n*Extracted from session new. Confidence: 0.5 (session-extracted).*",
            caller_attributed: true,
            session_scope_paths: &[],
            candidate_lookup: CandidateLookup::with_override(test_candidate_lookup),
        };
        let mut quality = ExtractionQuality::default();
        process_extracted_note(&context, "case", &test_extracted_case(), &mut quality).await;

        assert_eq!(quality.evidence_merged, 0);
        assert_eq!(quality.boost_fallback, 1);
        assert_eq!(quality.novelty_skipped, 1);
        assert_eq!(quality.merged, 1);
        let revisions = repo.revisions();
        assert_eq!(
            revisions
                .iter()
                .filter(|op| op.mutation.event_kind == NoteRevisionEventKind::ConfidenceChanged)
                .count(),
            1,
            "transport failure must retain exactly one confidence-only boost"
        );
        assert!(
            !revisions
                .iter()
                .any(|op| op.mutation.event_kind == NoteRevisionEventKind::Updated),
            "transport failure must not persist content"
        );
    }

    #[tokio::test]
    async fn content_update_failure_falls_back_without_aborting_extraction() {
        let original_content = test_existing_note().content;
        let provider = ScriptedProvider::new(vec![
            r#"{"decision":"already_known","existing_note_id":"existing-note-1"}"#.to_string(),
            r#"{"content":"Merged evidence that cannot be persisted because the controlled repository update fails."}"#.to_string(),
        ]);
        let repo = RecordingExtractionRepository::with_mutation_failure(
            test_existing_note(),
            NoteRevisionEventKind::Updated,
        );
        let context = ExtractionContext {
            note_repo: &repo,
            provider: &provider,
            project_id: "project-1",
            project_path: "/projects/project-1",
            knowledge_branch_target: &KnowledgeBranchTarget::Main,
            session_id: "new",
            task_id: "task-1",
            task_run_id: None,
            task_short_id: "t1",
            task_title: "Test task",
            task_description: "Test task description",
            provenance: "\n\n---\n*Extracted from session new. Confidence: 0.5 (session-extracted).*",
            caller_attributed: true,
            session_scope_paths: &[],
            candidate_lookup: CandidateLookup::with_override(test_candidate_lookup),
        };
        let mut quality = ExtractionQuality::default();
        process_extracted_note(&context, "case", &test_extracted_case(), &mut quality).await;

        assert_eq!(quality.evidence_merged, 0);
        assert_eq!(quality.boost_fallback, 1);
        assert_eq!(quality.novelty_skipped, 1);
        assert_eq!(quality.merged, 1);
        let revisions = repo.revisions();
        assert_eq!(
            revisions
                .iter()
                .filter(|op| op.mutation.event_kind == NoteRevisionEventKind::Updated)
                .count(),
            0,
            "the controlled repository must receive the failed persistence attempt"
        );
        assert_eq!(
            revisions
                .iter()
                .filter(|op| op.mutation.event_kind == NoteRevisionEventKind::ConfidenceChanged)
                .count(),
            1,
            "update failure must retain exactly one confidence-only boost"
        );
        assert_eq!(
            repo.existing_content(),
            original_content,
            "failed persistence must leave the existing content unchanged"
        );
    }

    #[tokio::test]
    async fn malformed_merge_response_falls_back_without_aborting_extraction() {
        let provider = ScriptedProvider::new(vec![
            r#"{"decision":"already_known","existing_note_id":"existing-note-1"}"#.to_string(),
            "not valid merge json".to_string(),
        ]);
        let repo = RecordingExtractionRepository::with_existing(test_existing_note());
        let context = ExtractionContext {
            note_repo: &repo,
            provider: &provider,
            project_id: "project-1",
            project_path: "/projects/project-1",
            knowledge_branch_target: &KnowledgeBranchTarget::Main,
            session_id: "new",
            task_id: "task-1",
            task_run_id: None,
            task_short_id: "t1",
            task_title: "Test task",
            task_description: "Test task description",
            provenance: "\n\n---\n*Extracted from session new. Confidence: 0.5 (session-extracted).*",
            caller_attributed: true,
            session_scope_paths: &[],
            candidate_lookup: CandidateLookup::with_override(test_candidate_lookup),
        };
        let mut quality = ExtractionQuality::default();
        process_extracted_note(&context, "case", &test_extracted_case(), &mut quality).await;

        assert_eq!(quality.evidence_merged, 0);
        assert_eq!(quality.boost_fallback, 1);
        // These counters describe the novelty decision, not whether the
        // evidence-merge response could be parsed or persisted. The malformed
        // response still classified this note as a duplicate before its
        // confidence-only fallback committed.
        assert_eq!(quality.novelty_skipped, 1);
        assert_eq!(quality.merged, 1);
        assert!(
            repo.revisions()
                .iter()
                .any(|op| op.mutation.event_kind == NoteRevisionEventKind::ConfidenceChanged)
        );
        assert!(
            !repo
                .revisions()
                .iter()
                .any(|op| op.mutation.event_kind == NoteRevisionEventKind::Updated)
        );
    }

    #[tokio::test]
    async fn protected_high_confidence_note_is_classified_as_boost_fallback() {
        let provider = ScriptedProvider::new(vec![
            r#"{"decision":"already_known","existing_note_id":"existing-note-1"}"#.to_string(),
        ]);
        let mut protected = test_existing_note();
        protected.confidence = EVIDENCE_MERGE_MAX_CONFIDENCE;
        let repo = RecordingExtractionRepository::with_existing(protected);
        let context = ExtractionContext {
            note_repo: &repo,
            provider: &provider,
            project_id: "project-1",
            project_path: "/projects/project-1",
            knowledge_branch_target: &KnowledgeBranchTarget::Main,
            session_id: "new",
            task_id: "task-1",
            task_run_id: None,
            task_short_id: "t1",
            task_title: "Test task",
            task_description: "Test task description",
            provenance: "footer",
            caller_attributed: true,
            session_scope_paths: &[],
            candidate_lookup: CandidateLookup::with_override(test_candidate_lookup),
        };
        let mut quality = ExtractionQuality::default();
        process_extracted_note(&context, "case", &test_extracted_case(), &mut quality).await;

        assert_eq!(quality.evidence_merged, 0);
        assert_eq!(quality.boost_fallback, 1);
        assert!(
            repo.revisions()
                .iter()
                .any(|op| op.mutation.event_kind == NoteRevisionEventKind::ConfidenceChanged)
        );
        assert!(
            !repo
                .revisions()
                .iter()
                .any(|op| op.mutation.event_kind == NoteRevisionEventKind::Updated)
        );
    }

    #[tokio::test]
    async fn replay_capture_uses_production_quality_dedup_and_unknown_fallback_without_sink() {
        let extracted = serde_json::json!({
            "cases": [{
                "title": test_extracted_case().title,
                "content": test_extracted_case().content,
            }],
            "patterns": [],
            "pitfalls": [],
        })
        .to_string();

        let duplicate_provider = ScriptedProvider::new(vec![
            r#"{"decision":"already_known","existing_note_id":"existing-note-1"}"#.to_string(),
        ]);
        let duplicate = capture_llm_extraction_replay(
            "duplicate".to_string(),
            &extracted,
            &duplicate_provider,
            &[test_candidate()],
        )
        .await
        .expect("capture duplicate replay");
        assert!(duplicate[0].adr_054_quality_passed);
        assert_eq!(
            duplicate[0].duplicate_of.as_deref(),
            Some("existing-note-1")
        );

        // Invalid novelty JSON has the same Unknown => non-duplicate fallback
        // as `process_extracted_note`; capture has no repository/sink argument,
        // so this assertion also exercises the non-persisting seam.
        let malformed_provider = ScriptedProvider::new(vec!["not-json".to_string()]);
        let malformed = capture_llm_extraction_replay(
            "malformed".to_string(),
            &extracted,
            &malformed_provider,
            &[test_candidate()],
        )
        .await
        .expect("capture malformed novelty replay");
        assert!(malformed[0].adr_054_quality_passed);
        assert_eq!(malformed[0].duplicate_of, None);

        let underspecified = r#"{"cases":[{"title":"temporary","content":"temporary task note"}],"patterns":[],"pitfalls":[]}"#;
        let quality = capture_llm_extraction_replay(
            "quality".to_string(),
            underspecified,
            &ScriptedProvider::new(vec![]),
            &[],
        )
        .await
        .expect("capture quality replay");
        assert!(!quality[0].adr_054_quality_passed);
    }
}
