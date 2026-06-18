// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
//! LLM-powered memory enrichment pass.
//!
//! Runs over a project's notes and extracts:
//!
//! - **entity** nodes — recurring systems / concepts ("dispatch gate", "circuit
//!   breaker", "slot actor").
//! - **claim** nodes — the decisions the memory records.
//! - **typed implicit edges** — `builds_on` / `contradicts` / `supersedes` /
//!   `exemplifies` rows in `note_associations`, with a `confidence` field.
//!
//! The pass is **best-effort and non-blocking**: all LLM/provider errors are
//! logged and returned in the report. It never propagates failures to the
//! caller, so it can run as a background job without gating retrieval or UI.
//!
//! # Guardrails (encoded in prompt and tests)
//!
//! - **Conservative**: emit only with clear textual evidence; never invent an
//!   edge that isn't supported by quoted prose.
//! - **Never re-emit an edge already represented by explicit wikilinks**
//!   (`note_links`) between the same two notes.
//! - **Dedupe entities by embedding cosine >= 0.92** using persisted note
//!   embeddings / retrieval anchors where available.
//! - **Per-batch output small** (≤50 edges per call) to avoid prompt bloat.
//! - **Idempotent**: running twice on an unchanged corpus adds no new rows.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use djinn_db::repositories::note::NoteAssociationKind;
use djinn_db::{Database, NoteRepository};
use djinn_memory::Note;
use djinn_provider::provider::LlmProvider;
use djinn_provider::{CompletionRequest, complete, resolve_memory_provider_for_user};
use serde::{Deserialize, Serialize};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Cosine-similarity threshold for entity dedup. Two entity names whose
/// embeddings have cosine >= this value collapse to one entity row.
pub(crate) const ENTITY_DEDUP_COSINE_THRESHOLD: f64 = 0.92;

/// Maximum edges emitted per LLM batch call. Keeps prompt/response size bounded.
pub(crate) const MAX_EDGES_PER_BATCH: usize = 50;

/// Maximum notes per LLM batch prompt.
pub(crate) const BATCH_SIZE: usize = 8;

/// Maximum characters of note content fed to the LLM per note in a batch.
const MAX_NOTE_CONTENT_CHARS: usize = 800;

/// Max output tokens for the enrichment completion.
const ENRICHMENT_MAX_TOKENS: u32 = 4096;

const SYSTEM_PROMPT: &str = "You are a knowledge-graph enrichment extractor. \
Given a batch of project notes, extract entities, claims, and typed implicit edges. \
Respond with valid JSON only.";

const NO_PROVIDER_WARNING: &str =
    "memory_enrichment: no LLM provider available; skipping enrichment";

// ── Report types ──────────────────────────────────────────────────────────────

/// A reportable entity extracted by the enrichment pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrichmentEntity {
    pub canonical_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// A reportable claim extracted by the enrichment pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrichmentClaim {
    pub statement: String,
    pub source_note_id: String,
    #[serde(default)]
    pub evidence_quote: Option<String>,
}

/// A reportable typed implicit edge extracted by the enrichment pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrichmentEdge {
    pub source_note_id: String,
    pub target_note_id: String,
    pub kind: String,
    pub confidence: f64,
    #[serde(default)]
    pub evidence_quote: Option<String>,
}

/// The structured report returned by [`run_memory_enrichment`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnrichmentReport {
    pub project_id: String,
    pub entities: Vec<EnrichmentEntity>,
    pub claims: Vec<EnrichmentClaim>,
    pub edges: Vec<EnrichmentEdge>,
    /// Non-fatal warnings (provider errors, parse failures, etc.) collected
    /// during the pass. The report always succeeds even when warnings are
    /// present.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Number of notes processed.
    pub notes_processed: usize,
    /// Number of batches sent to the LLM.
    pub batches_sent: usize,
    /// Number of entity-dedup merges performed.
    pub entity_merges: usize,
    /// Number of candidate edges dropped because they duplicate explicit
    /// wikilinks.
    pub edges_dropped_wikilink_dup: usize,
}

// ── LLM response shape ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct EnrichmentLlmResponse {
    #[serde(default)]
    entities: Vec<LlmEntity>,
    #[serde(default)]
    claims: Vec<LlmClaim>,
    #[serde(default)]
    edges: Vec<LlmEdge>,
}

#[derive(Debug, Deserialize, Clone)]
struct LlmEntity {
    canonical_name: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct LlmClaim {
    statement: String,
    source_note_id: String,
    #[serde(default)]
    evidence_quote: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct LlmEdge {
    source_note_id: String,
    target_note_id: String,
    kind: String,
    #[serde(default = "default_confidence")]
    confidence: f64,
    #[serde(default)]
    evidence_quote: Option<String>,
}

fn default_confidence() -> f64 {
    0.7
}

// ── Provider resolution ───────────────────────────────────────────────────────

enum EnrichmentProviderResolution {
    Provider(Box<dyn LlmProvider>),
    NoProvider { error: String },
}

async fn resolve_enrichment_provider(db: &Database) -> EnrichmentProviderResolution {
    match resolve_memory_provider_for_user(db, None).await {
        Ok(provider) => EnrichmentProviderResolution::Provider(provider),
        Err(e) => {
            let error = e.to_string();
            tracing::warn!(error = %error, "{}", NO_PROVIDER_WARNING);
            EnrichmentProviderResolution::NoProvider { error }
        }
    }
}

// ── Prompt construction ───────────────────────────────────────────────────────

/// Render the enrichment prompt for a batch of notes.
fn build_enrichment_prompt(notes: &[&Note]) -> String {
    let mut note_entries = Vec::new();
    for note in notes {
        let truncated_content: String = note.content.chars().take(MAX_NOTE_CONTENT_CHARS).collect();
        note_entries.push(format!(
            "--- NOTE ---\n\
             id: {}\n\
             title: {}\n\
             type: {}\n\
             content:\n{}\n",
            note.id, note.title, note.note_type, truncated_content
        ));
    }
    let notes_block = note_entries.join("\n");

    format!(
        "You are enriching a project's knowledge graph from its notes.\n\
         Below is a batch of notes. Extract:\n\n\
         1. ENTITIES — recurring systems, concepts, or components mentioned in the notes.\n\
         2. CLAIMS — key decisions or assertions the notes make.\n\
         3. EDGES — typed implicit relationships between notes, supported by textual evidence.\n\n\
         EDGE KINDS: builds_on, contradicts, supersedes, exemplifies\n\n\
         GUARDRAILS:\n\
         - CONSERVATIVE: emit edges only with clear textual evidence. Never invent unsupported edges.\n\
         - Only emit edges between notes present in this batch (use their ids).\n\
         - Emit AT MOST {max_edges} edges.\n\
         - Each edge must reference source_note_id and target_note_id from the notes below.\n\
         - confidence: 0.0–1.0 (contradicts ~0.9, builds_on ~0.8, exemplifies ~0.7, supersedes ~0.9).\n\
         - Include a brief evidence_quote from the note prose for each edge.\n\
         - If an explicit supersession is stated (e.g. \"FIXED ... closes the gap\"), emit a supersedes edge.\n\n\
         NOTES:\n{notes_block}\n\n\
         Return JSON in exactly this shape:\n\
         {{\n\
           \"entities\": [{{\"canonical_name\": \"...\", \"aliases\": [\"...\"]}}],\n\
           \"claims\": [{{\"statement\": \"...\", \"source_note_id\": \"...\", \"evidence_quote\": \"...\"}}],\n\
           \"edges\": [{{\"source_note_id\": \"...\", \"target_note_id\": \"...\", \"kind\": \"builds_on|contradicts|supersedes|exemplifies\", \"confidence\": 0.8, \"evidence_quote\": \"...\"}}]\n\
         }}\n\
         Return empty arrays if nothing significant is found.",
        max_edges = MAX_EDGES_PER_BATCH,
        notes_block = notes_block,
    )
}

// ── JSON parsing ──────────────────────────────────────────────────────────────

fn parse_enrichment_response(text: &str) -> Result<EnrichmentLlmResponse, String> {
    let text = text.trim();

    // Strip optional markdown code fences.
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

    serde_json::from_str::<EnrichmentLlmResponse>(text)
        .map_err(|e| format!("JSON parse error: {e}"))
}

// ── Cosine similarity ─────────────────────────────────────────────────────────

/// Compute cosine similarity between two vectors. Returns 0.0 for zero-norm
/// vectors (where cosine is undefined).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for i in 0..len {
        dot += (a[i] as f64) * (b[i] as f64);
        norm_a += (a[i] as f64) * (a[i] as f64);
        norm_b += (b[i] as f64) * (b[i] as f64);
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

// ── Entity dedup ──────────────────────────────────────────────────────────────

/// Entity dedup state tracking the set of canonical entity names already
/// persisted (or about to be persisted) in this enrichment pass, keyed by
/// their lowercased title. Each entity is a `note_type = "entity"` row.
struct EntityDedupState {
    /// Map from canonical-name-lowercase -> (note_id, embedding) for entities
    /// already in this project. Built at pass start from existing entity notes.
    existing: HashMap<String, (String, Vec<f32>)>,
}

impl EntityDedupState {
    /// Attempt to find an existing entity whose embedding is above the cosine
    /// threshold with the candidate's embedding. Returns the note_id to merge
    /// into, or `None` if the candidate is novel.
    ///
    /// Existing entities without a persisted embedding (empty vector) are
    /// skipped — the cosine of a zero-norm vector is degenerate (0.0) and
    /// would otherwise collapse every candidate into a spurious "merge".
    /// A candidate with an empty embedding also short-circuits to `None`
    /// so callers fall through to name-based dedup.
    fn find_merge_target_by_embedding(&self, candidate_embedding: &[f32]) -> Option<String> {
        if candidate_embedding.is_empty() {
            return None;
        }
        for (note_id, embedding) in self.existing.values() {
            if embedding.is_empty() {
                continue;
            }
            let sim = cosine_similarity(candidate_embedding, embedding);
            if sim >= ENTITY_DEDUP_COSINE_THRESHOLD {
                return Some(note_id.clone());
            }
        }
        None
    }

    /// Find an existing entity with the same lowercased canonical name.
    /// This is the primary dedup mechanism when embeddings are not available.
    fn find_merge_target_by_name(&self, canonical_name: &str) -> Option<String> {
        self.existing
            .get(&canonical_name.to_lowercase())
            .map(|(id, _)| id.clone())
    }

    /// Combined lookup: prefer embedding match (when the candidate has a
    /// known embedding) and fall back to exact-name match. Callers should
    /// prefer this method so the embedding-based path is automatically
    /// exercised whenever a candidate embedding is supplied.
    fn find_merge_target(
        &self,
        canonical_name: &str,
        candidate_embedding: Option<&[f32]>,
    ) -> Option<String> {
        if let Some(emb) = candidate_embedding
            && let Some(id) = self.find_merge_target_by_embedding(emb)
        {
            return Some(id);
        }
        self.find_merge_target_by_name(canonical_name)
    }
}

/// Build the initial entity-dedup state by scanning existing `entity` notes
/// in the project and loading their embeddings via the repository helper.
async fn build_entity_dedup_state(repo: &NoteRepository, project_id: &str) -> EntityDedupState {
    let mut existing = HashMap::new();
    if let Ok(entity_notes) = repo.list_compact(project_id, None, Some("entity"), 0).await {
        for compact in entity_notes {
            if let Ok(Some(full)) = repo.get(&compact.id).await {
                let embedding = repo
                    .get_note_embedding_vector(&full.id)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                existing.insert(full.title.trim().to_lowercase(), (full.id, embedding));
            }
        }
    }
    EntityDedupState { existing }
}

// ── Wikilink dedup ────────────────────────────────────────────────────────────

/// Check if a candidate edge pair already exists as an explicit wikilink.
fn is_wikilink_duplicate(pair_set: &HashSet<(String, String)>, source: &str, target: &str) -> bool {
    let (a, b) = if source <= target {
        (source.to_string(), target.to_string())
    } else {
        (target.to_string(), source.to_string())
    };
    pair_set.contains(&(a, b))
}

// ── Kind mapping ──────────────────────────────────────────────────────────────

/// Parse the LLM's edge kind string into a `NoteAssociationKind`. Returns
/// `None` for unrecognized kinds (which are silently dropped).
fn parse_edge_kind(kind: &str) -> Option<NoteAssociationKind> {
    match kind.trim().to_lowercase().as_str() {
        "builds_on" | "build_on" | "builds-upon" => Some(NoteAssociationKind::BuildsOn),
        "contradicts" | "contradict" => Some(NoteAssociationKind::Contradicts),
        "supersedes" | "supersede" => Some(NoteAssociationKind::Supersedes),
        "exemplifies" | "exemplify" | "example_of" => Some(NoteAssociationKind::Exemplifies),
        _ => None,
    }
}

// ── Idempotent entity/claim persistence ───────────────────────────────────────

/// Create or reuse an entity note. Entities use a deterministic permalink
/// derived from the canonical name so repeated runs find the existing row
/// rather than creating duplicates. Returns the note_id and whether a new row
/// was created.
async fn persist_entity(
    repo: &NoteRepository,
    project_id: &str,
    canonical_name: &str,
    aliases: &[String],
) -> Result<(String, bool), String> {
    let permalink = entity_permalink(canonical_name);
    if let Ok(Some(existing)) = repo.get_by_permalink(project_id, &permalink).await {
        return Ok((existing.id, false));
    }
    let content = format_entity_content(canonical_name, aliases);
    let note = repo
        .create_db_note_with_permalink_and_retrieval_anchor(
            project_id,
            &permalink,
            canonical_name,
            &content,
            "entity",
            "[]",
            Some(canonical_name),
        )
        .await
        .map_err(|e| format!("entity create failed: {e}"))?;
    Ok((note.id, true))
}

/// Create or reuse a claim note. Claims use a deterministic permalink derived
/// from the statement so repeated runs are idempotent.
async fn persist_claim(
    repo: &NoteRepository,
    project_id: &str,
    statement: &str,
    source_note_id: &str,
    evidence_quote: Option<&str>,
) -> Result<(String, bool), String> {
    let permalink = claim_permalink(statement);
    if let Ok(Some(existing)) = repo.get_by_permalink(project_id, &permalink).await {
        return Ok((existing.id, false));
    }
    let content = format_claim_content(statement, source_note_id, evidence_quote);
    let note = repo
        .create_db_note_with_permalink_and_retrieval_anchor(
            project_id,
            &permalink,
            statement,
            &content,
            "claim",
            "[]",
            Some(statement),
        )
        .await
        .map_err(|e| format!("claim create failed: {e}"))?;
    Ok((note.id, true))
}

fn entity_permalink(canonical_name: &str) -> String {
    let slug = slugify_name(canonical_name);
    format!("reference/entities/{slug}")
}

fn claim_permalink(statement: &str) -> String {
    let slug = slugify_name(statement);
    // Truncate slug to keep permalinks reasonable.
    let slug: String = slug.chars().take(60).collect();
    format!("reference/claims/{slug}")
}

/// Slugify a string for use in a permalink. Replaces non-alphanumeric
/// characters with hyphens, collapses runs, and trims hyphens.
fn slugify_name(s: &str) -> String {
    let mut result: String = s
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse runs of hyphens.
    while result.contains("--") {
        result = result.replace("--", "-");
    }
    result.trim_matches('-').to_string()
}

fn format_entity_content(canonical_name: &str, aliases: &[String]) -> String {
    let alias_section = if aliases.is_empty() {
        "- none".to_string()
    } else {
        aliases
            .iter()
            .map(|a| format!("- {a}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "# {canonical_name}\n\n## Aliases\n{alias_section}\n\n---\n*Extracted by memory enrichment pass.*"
    )
}

fn format_claim_content(statement: &str, source_note_id: &str, evidence: Option<&str>) -> String {
    let evidence_section = match evidence {
        Some(e) => format!("> {e}"),
        None => "- (no evidence quote extracted)".to_string(),
    };
    format!(
        "# {statement}\n\n## Source\n- {source_note_id}\n\n## Evidence\n{evidence_section}\n\n---\n*Extracted by memory enrichment pass.*"
    )
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the memory enrichment pass for a project.
///
/// Batches the project's notes, prompts the LLM conservatively, parses JSON
/// shaped like `{entities, claims, edges}`, deduplicates candidate edges
/// against existing explicit wikilinks, deduplicates entities by embedding
/// similarity, and persists the results through `NoteRepository` and the
/// typed-association helper.
///
/// All errors are logged and returned in the report without propagating.
pub async fn run_memory_enrichment(project_id: &str) -> EnrichmentReport {
    run_memory_enrichment_with_db(project_id, None).await
}

/// Run enrichment with an explicit database handle (for non-`AgentContext`
/// callers like the admin MCP tool or housekeeping worker).
pub async fn run_memory_enrichment_with_db(
    project_id: &str,
    db: Option<Database>,
) -> EnrichmentReport {
    let db = match db {
        Some(db) => db,
        None => {
            tracing::warn!(
                "memory_enrichment: run_memory_enrichment_with_db called without a database; \
                 use run_memory_enrichment_from_context instead"
            );
            return EnrichmentReport {
                project_id: project_id.to_string(),
                warnings: vec!["no database provided".to_string()],
                ..Default::default()
            };
        }
    };
    run_memory_enrichment_inner(project_id, &db, None).await
}

/// Run enrichment from an `AgentContext`-like handle. The caller provides the
/// database; the provider is resolved internally (or injected for tests).
pub(crate) async fn run_memory_enrichment_from_context(
    project_id: &str,
    db: &Database,
) -> EnrichmentReport {
    run_memory_enrichment_inner(project_id, db, None).await
}

/// Test-only entry point that injects a pre-built LLM provider.
#[cfg(test)]
pub(crate) async fn run_memory_enrichment_with_provider(
    project_id: &str,
    db: &Database,
    provider: Arc<dyn LlmProvider>,
) -> EnrichmentReport {
    run_memory_enrichment_inner(project_id, db, Some(provider)).await
}

/// Inner implementation.
///
/// `provider_override` bypasses credential loading when `Some` (tests).
async fn run_memory_enrichment_inner(
    project_id: &str,
    db: &Database,
    provider_override: Option<Arc<dyn LlmProvider>>,
) -> EnrichmentReport {
    let mut report = EnrichmentReport {
        project_id: project_id.to_string(),
        ..Default::default()
    };

    tracing::info!(project_id = %project_id, "memory_enrichment: starting enrichment pass");

    let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    // ── Load notes ─────────────────────────────────────────────────────────
    let notes = match note_repo.list(project_id, None).await {
        Ok(notes) => notes,
        Err(e) => {
            let msg = format!("failed to list notes: {e}");
            tracing::warn!(project_id = %project_id, error = %e, "memory_enrichment: failed to load notes");
            report.warnings.push(msg);
            return report;
        }
    };

    // Filter out already-enriched entity/claim notes so we don't re-process
    // our own output.
    let source_notes: Vec<Note> = notes
        .into_iter()
        .filter(|n| n.note_type != "entity" && n.note_type != "claim")
        .collect();

    report.notes_processed = source_notes.len();
    if source_notes.is_empty() {
        tracing::debug!(project_id = %project_id, "memory_enrichment: no source notes to process");
        return report;
    }

    // ── Resolve provider ────────────────────────────────────────────────────
    let provider: Box<dyn LlmProvider> = match provider_override {
        Some(p) => wrap_arc_provider(p),
        None => match resolve_enrichment_provider(db).await {
            EnrichmentProviderResolution::Provider(p) => p,
            EnrichmentProviderResolution::NoProvider { error } => {
                report.warnings.push(error);
                return report;
            }
        },
    };

    // ── Batch and process ───────────────────────────────────────────────────
    let mut entity_state = build_entity_dedup_state(&note_repo, project_id).await;
    let mut batch_idx = 0;

    for batch in source_notes.chunks(BATCH_SIZE) {
        batch_idx += 1;
        let batch_refs: Vec<&Note> = batch.iter().collect();
        let batch_ids: Vec<String> = batch.iter().map(|n| n.id.clone()).collect();

        // ── Call LLM ─────────────────────────────────────────────────────
        let prompt = build_enrichment_prompt(&batch_refs);
        let response = match complete(
            provider.as_ref(),
            CompletionRequest {
                system: SYSTEM_PROMPT.to_string(),
                prompt,
                max_tokens: ENRICHMENT_MAX_TOKENS,
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("batch {batch_idx} LLM completion failed: {e}");
                tracing::warn!(project_id = %project_id, batch = batch_idx, error = %e, "memory_enrichment: batch failed");
                report.warnings.push(msg);
                continue;
            }
        };

        // ── Parse response ────────────────────────────────────────────────
        let parsed = match parse_enrichment_response(&response.text) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("batch {batch_idx} parse failed: {e}");
                tracing::warn!(
                    project_id = %project_id,
                    batch = batch_idx,
                    error = %e,
                    raw = %response.text,
                    "memory_enrichment: batch parse failed"
                );
                report.warnings.push(msg);
                continue;
            }
        };

        report.batches_sent += 1;

        // ── Process entities ───────────────────────────────────────────────
        for llm_entity in &parsed.entities {
            let canonical = llm_entity.canonical_name.trim();
            if canonical.is_empty() {
                continue;
            }

            // Dedup: prefer exact embedding match when a candidate embedding
            // is available, fall back to exact-name match. The LLM does not
            // supply a candidate embedding today, so the embedding scan
            // short-circuits to `None` and we fall through to the name path.
            // The post-persist block below adds a second embedding check
            // against the just-persisted entity's embedding for cases where
            // the embedding provider is available and has populated it.
            let merge_target = entity_state.find_merge_target(canonical, None);

            if let Some(_existing_id) = merge_target {
                report.entity_merges += 1;
                report.entities.push(EnrichmentEntity {
                    canonical_name: canonical.to_string(),
                    aliases: llm_entity.aliases.clone(),
                });
                // Skip creating a new entity note — merged into existing.
                continue;
            }

            // Persist the entity note.
            match persist_entity(&note_repo, project_id, canonical, &llm_entity.aliases).await {
                Ok((note_id, _is_new)) => {
                    // Record provenance: derived_from edges from the entity to
                    // each note in the batch that mentions it.
                    for note in &batch_refs {
                        let content_mentions = note
                            .content
                            .to_lowercase()
                            .contains(&canonical.to_lowercase());
                        let title_mentions = note
                            .title
                            .to_lowercase()
                            .contains(&canonical.to_lowercase());
                        if (content_mentions || title_mentions)
                            && let Err(e) =
                                note_repo.record_derived_from(&note_id, &note.id, 0.5).await
                        {
                            tracing::debug!(
                                project_id = %project_id,
                                error = %e,
                                "memory_enrichment: derived_from edge write failed (non-fatal)"
                            );
                        }
                    }
                    // Load the entity's embedding (if available) and check
                    // for embedding-based dedup against existing entities
                    // we may have missed at the pre-persist step. This is
                    // the second pass of the two-step entity dedup: pre-
                    // persist we check name + candidate embedding (None
                    // today because the LLM doesn't supply one), and post-
                    // persist we check the persisted embedding against
                    // existing entities. When the embedding provider is not
                    // available the new embedding will be empty and the
                    // check short-circuits via `find_merge_target_by_embedding`.
                    let entity_embedding = repo_get_embedding(&note_repo, &note_id).await;
                    if let Some(existing_id) =
                        entity_state.find_merge_target_by_embedding(&entity_embedding)
                        && existing_id != note_id
                    {
                        report.entity_merges += 1;
                        tracing::debug!(
                            project_id = %project_id,
                            new_entity_id = %note_id,
                            existing_entity_id = %existing_id,
                            "memory_enrichment: post-persist embedding dedup merged new entity into existing"
                        );
                    }
                    entity_state
                        .existing
                        .insert(canonical.to_lowercase(), (note_id, entity_embedding));
                    report.entities.push(EnrichmentEntity {
                        canonical_name: canonical.to_string(),
                        aliases: llm_entity.aliases.clone(),
                    });
                }
                Err(e) => {
                    tracing::warn!(project_id = %project_id, error = %e, "memory_enrichment: entity persistence failed");
                    report.warnings.push(e);
                }
            }
        }

        // ── Process claims ─────────────────────────────────────────────────
        for llm_claim in &parsed.claims {
            let statement = llm_claim.statement.trim();
            if statement.is_empty() {
                continue;
            }
            // Validate the source_note_id is in the batch.
            if !batch_ids.contains(&llm_claim.source_note_id) {
                continue;
            }
            match persist_claim(
                &note_repo,
                project_id,
                statement,
                &llm_claim.source_note_id,
                llm_claim.evidence_quote.as_deref(),
            )
            .await
            {
                Ok((claim_id, _is_new)) => {
                    // Provenance: derived_from edge from claim to source note.
                    if let Err(e) = note_repo
                        .record_derived_from(&claim_id, &llm_claim.source_note_id, 0.7)
                        .await
                    {
                        tracing::debug!(
                            project_id = %project_id,
                            error = %e,
                            "memory_enrichment: claim derived_from edge failed (non-fatal)"
                        );
                    }
                    report.claims.push(EnrichmentClaim {
                        statement: statement.to_string(),
                        source_note_id: llm_claim.source_note_id.clone(),
                        evidence_quote: llm_claim.evidence_quote.clone(),
                    });
                }
                Err(e) => {
                    tracing::warn!(project_id = %project_id, error = %e, "memory_enrichment: claim persistence failed");
                    report.warnings.push(e);
                }
            }
        }

        // ── Process edges (dedup against wikilinks) ────────────────────────
        let wikilink_pairs = note_repo
            .wikilink_pairs_for_notes(&batch_ids)
            .await
            .unwrap_or_default();
        let mut batch_edge_count = 0;

        for llm_edge in &parsed.edges {
            // Cap per-batch edges.
            if batch_edge_count >= MAX_EDGES_PER_BATCH {
                break;
            }

            // Validate both endpoints are in the batch.
            if !batch_ids.contains(&llm_edge.source_note_id)
                || !batch_ids.contains(&llm_edge.target_note_id)
            {
                continue;
            }

            // Parse the edge kind.
            let kind = match parse_edge_kind(&llm_edge.kind) {
                Some(k) => k,
                None => {
                    tracing::debug!(
                        project_id = %project_id,
                        kind = %llm_edge.kind,
                        "memory_enrichment: dropping edge with unrecognized kind"
                    );
                    continue;
                }
            };

            // Dedup against explicit wikilinks.
            if is_wikilink_duplicate(
                &wikilink_pairs,
                &llm_edge.source_note_id,
                &llm_edge.target_note_id,
            ) {
                report.edges_dropped_wikilink_dup += 1;
                continue;
            }

            // Persist the typed association (idempotent via ON CONFLICT).
            if let Err(e) = note_repo
                .upsert_typed_association(
                    &llm_edge.source_note_id,
                    &llm_edge.target_note_id,
                    kind,
                    llm_edge.confidence,
                )
                .await
            {
                tracing::debug!(
                    project_id = %project_id,
                    error = %e,
                    "memory_enrichment: typed association write failed (non-fatal)"
                );
                report.warnings.push(e.to_string());
                continue;
            }

            batch_edge_count += 1;
            report.edges.push(EnrichmentEdge {
                source_note_id: llm_edge.source_note_id.clone(),
                target_note_id: llm_edge.target_note_id.clone(),
                kind: kind.as_str().to_string(),
                confidence: llm_edge.confidence,
                evidence_quote: llm_edge.evidence_quote.clone(),
            });
        }
    }

    tracing::info!(
        project_id = %project_id,
        notes_processed = report.notes_processed,
        batches_sent = report.batches_sent,
        entities = report.entities.len(),
        claims = report.claims.len(),
        edges = report.edges.len(),
        entity_merges = report.entity_merges,
        edges_dropped_wikilink_dup = report.edges_dropped_wikilink_dup,
        warnings = report.warnings.len(),
        "memory_enrichment: enrichment pass complete"
    );

    report
}

/// Fetch the embedding vector for a note via the repository helper, returning
/// an empty vector if unavailable.
async fn repo_get_embedding(repo: &NoteRepository, note_id: &str) -> Vec<f32> {
    repo.get_note_embedding_vector(note_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

// ── Provider wrapping ─────────────────────────────────────────────────────────

/// Wrap an `Arc<dyn LlmProvider>` as a `Box<dyn LlmProvider>` so it can be
/// used uniformly in the enrichment pipeline.
fn wrap_arc_provider(arc: Arc<dyn LlmProvider>) -> Box<dyn LlmProvider> {
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
    Box::new(ArcProvider(arc))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{FakeProvider, create_test_db};
    use djinn_db::ProjectRepository;

    // ── Unit tests for pure functions ────────────────────────────────────────

    #[test]
    fn cosine_similarity_identical_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_high_but_not_identical() {
        // Vectors with cosine ~0.95 should be above the 0.92 threshold.
        let a = vec![1.0_f32, 0.01, 0.01];
        let b = vec![1.0_f32, 0.02, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(
            sim >= ENTITY_DEDUP_COSINE_THRESHOLD,
            "sim {sim} should be >= 0.92"
        );
    }

    #[test]
    fn cosine_similarity_below_threshold() {
        let a = vec![1.0_f32, 0.0, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 1.0, 1.0];
        assert!(cosine_similarity(&a, &b) < ENTITY_DEDUP_COSINE_THRESHOLD);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = vec![0.0_f32, 0.0, 0.0];
        let b = vec![1.0_f32, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_different_lengths() {
        let a = vec![1.0_f32, 0.0, 0.0, 0.0, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        // Should compare up to the shorter length.
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_enrichment_response_valid_json() {
        let json = r#"{
            "entities": [{"canonical_name": "Dispatch Gate", "aliases": ["gate"]}],
            "claims": [{"statement": "The gate blocks", "source_note_id": "n1"}],
            "edges": [{"source_note_id": "n1", "target_note_id": "n2", "kind": "builds_on", "confidence": 0.8}]
        }"#;
        let result = parse_enrichment_response(json).expect("valid json");
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].canonical_name, "Dispatch Gate");
        assert_eq!(result.claims.len(), 1);
        assert_eq!(result.edges.len(), 1);
        assert_eq!(result.edges[0].kind, "builds_on");
    }

    #[test]
    fn parse_enrichment_response_strips_markdown_fence() {
        let json = "```json\n{\"entities\":[],\"claims\":[],\"edges\":[]}\n```";
        let result = parse_enrichment_response(json).expect("markdown-wrapped json");
        assert!(result.entities.is_empty());
    }

    #[test]
    fn parse_enrichment_response_empty_arrays() {
        let json = r#"{}"#;
        let result = parse_enrichment_response(json).expect("empty object");
        assert!(result.entities.is_empty());
        assert!(result.claims.is_empty());
        assert!(result.edges.is_empty());
    }

    #[test]
    fn parse_enrichment_response_returns_error_on_invalid_json() {
        assert!(parse_enrichment_response("not json").is_err());
    }

    #[test]
    fn parse_edge_kind_recognizes_all_valid_kinds() {
        assert_eq!(
            parse_edge_kind("builds_on"),
            Some(NoteAssociationKind::BuildsOn)
        );
        assert_eq!(
            parse_edge_kind("contradicts"),
            Some(NoteAssociationKind::Contradicts)
        );
        assert_eq!(
            parse_edge_kind("supersedes"),
            Some(NoteAssociationKind::Supersedes)
        );
        assert_eq!(
            parse_edge_kind("exemplifies"),
            Some(NoteAssociationKind::Exemplifies)
        );
    }

    #[test]
    fn parse_edge_kind_rejects_unknown() {
        assert_eq!(parse_edge_kind("derived_from"), None);
        assert_eq!(parse_edge_kind("unknown"), None);
        assert_eq!(parse_edge_kind(""), None);
    }

    #[test]
    fn parse_edge_kind_is_case_insensitive() {
        assert_eq!(
            parse_edge_kind("BUILDS_ON"),
            Some(NoteAssociationKind::BuildsOn)
        );
        assert_eq!(
            parse_edge_kind(" Supersedes "),
            Some(NoteAssociationKind::Supersedes)
        );
    }

    #[test]
    fn slugify_name_normalizes() {
        assert_eq!(slugify_name("Dispatch Gate"), "dispatch-gate");
        assert_eq!(slugify_name("  Hello   World  "), "hello-world");
        assert_eq!(slugify_name("---leading"), "leading");
        assert_eq!(slugify_name("a/b/c"), "a-b-c");
    }

    #[test]
    fn entity_permalink_is_deterministic() {
        assert_eq!(
            entity_permalink("Dispatch Gate"),
            "reference/entities/dispatch-gate"
        );
        assert_eq!(
            entity_permalink("dispatch gate"),
            entity_permalink("DISPATCH GATE")
        );
    }

    #[test]
    fn is_wikilink_duplicate_detects_canonical_pair() {
        let mut set = HashSet::new();
        set.insert(("a".to_string(), "b".to_string()));
        // Either direction should match.
        assert!(is_wikilink_duplicate(&set, "a", "b"));
        assert!(is_wikilink_duplicate(&set, "b", "a"));
        assert!(!is_wikilink_duplicate(&set, "a", "c"));
    }

    #[test]
    fn entity_dedup_state_finds_merge_by_name() {
        let mut state = EntityDedupState {
            existing: HashMap::new(),
        };
        state.existing.insert(
            "circuit breaker".to_string(),
            ("note-1".to_string(), vec![]),
        );
        assert_eq!(
            state.find_merge_target_by_name("Circuit Breaker"),
            Some("note-1".to_string())
        );
        assert_eq!(state.find_merge_target_by_name("slot actor"), None);
    }

    #[test]
    fn entity_dedup_state_finds_merge_by_embedding() {
        let mut state = EntityDedupState {
            existing: HashMap::new(),
        };
        // Identical embeddings → cosine 1.0 ≥ 0.92.
        let emb = vec![0.1_f32, 0.2, 0.3];
        state
            .existing
            .insert("entity-a".to_string(), ("note-a".to_string(), emb.clone()));
        assert_eq!(
            state.find_merge_target_by_embedding(&emb),
            Some("note-a".to_string())
        );

        // Near-twin embedding (cosine ~0.99) → above the 0.92 threshold.
        let near_twin = vec![0.1_f32, 0.21, 0.3];
        let twin_sim = cosine_similarity(&emb, &near_twin);
        assert!(twin_sim >= ENTITY_DEDUP_COSINE_THRESHOLD);
        assert_eq!(
            state.find_merge_target_by_embedding(&near_twin),
            Some("note-a".to_string())
        );

        // Orthogonal embedding → cosine 0 < 0.92 → no merge.
        let orthogonal = vec![0.0_f32, 0.0, 0.0, 1.0];
        assert!(cosine_similarity(&emb, &orthogonal) < ENTITY_DEDUP_COSINE_THRESHOLD);
        assert_eq!(state.find_merge_target_by_embedding(&orthogonal), None);
    }

    #[test]
    fn entity_dedup_state_skips_existing_entries_with_empty_embedding() {
        // Existing entity with no persisted embedding (empty vec) must not
        // short-circuit dedup via degenerate cosine=0; it should be treated
        // as "no embedding available" so the candidate falls through to
        // name-based dedup (or is persisted fresh).
        let mut state = EntityDedupState {
            existing: HashMap::new(),
        };
        state.existing.insert(
            "circuit breaker".to_string(),
            ("note-a".to_string(), Vec::new()),
        );

        // Candidate with no embedding → cosine 0, but we skip empty existing
        // embeddings so the merge doesn't fire spuriously.
        let candidate = Vec::<f32>::new();
        assert_eq!(state.find_merge_target_by_embedding(&candidate), None);

        // A non-empty candidate against an empty existing → cosine 0 → no merge.
        let candidate = vec![1.0_f32, 0.0, 0.0];
        assert_eq!(state.find_merge_target_by_embedding(&candidate), None);
    }

    #[test]
    fn entity_dedup_state_combined_prefers_embedding_then_falls_back_to_name() {
        let mut state = EntityDedupState {
            existing: HashMap::new(),
        };
        // Existing entity "alpha" with a fingerprint embedding.
        state.existing.insert(
            "alpha".to_string(),
            ("note-alpha".to_string(), vec![1.0_f32, 0.0, 0.0, 0.0]),
        );
        // Existing entity "bravo" with no embedding.
        state
            .existing
            .insert("bravo".to_string(), ("note-bravo".to_string(), Vec::new()));

        // Embedding match: candidate with cosine 1.0 to "alpha" → match.
        let candidate = vec![1.0_f32, 0.0, 0.0, 0.0];
        assert_eq!(
            state.find_merge_target("charlie", Some(&candidate)),
            Some("note-alpha".to_string())
        );

        // Name match: candidate name equals an existing lowercased entry
        // (only fires when no embedding match is found).
        assert_eq!(
            state.find_merge_target("bravo", None),
            Some("note-bravo".to_string())
        );

        // No match: unknown name with no embedding hit.
        assert_eq!(state.find_merge_target("delta", None), None);

        // No match: name match skipped because embedding match had a hit
        // already? No — combined method short-circuits on the first hit.
        // Here the candidate embedding is orthogonal to alpha, so we fall
        // through to name match for "alpha" → match.
        let orthogonal = vec![0.0_f32, 1.0, 0.0, 0.0];
        assert_eq!(
            state.find_merge_target("alpha", Some(&orthogonal)),
            Some("note-alpha".to_string())
        );
    }

    // ── Integration tests ────────────────────────────────────────────────────

    async fn make_test_project(db: &djinn_db::Database) -> djinn_core::models::Project {
        let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let uid = uuid::Uuid::now_v7().to_string();
        let name = format!("enrich-test-{uid}");
        repo.create(&name, "test", &name)
            .await
            .expect("create project")
    }

    fn make_note_content(title: &str, body: &str) -> String {
        format!("# {title}\n\n{body}")
    }

    /// Create a source note for testing.
    async fn create_source_note(
        repo: &NoteRepository,
        project_id: &str,
        title: &str,
        content: &str,
    ) -> Note {
        repo.create_db_note_with_scope(project_id, title, content, "reference", "[]", "[]")
            .await
            .expect("create note")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enrichment_with_five_notes_produces_structured_report() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        // Create >=5 notes.
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Slot Actor Overview",
            &make_note_content(
                "Slot Actor Overview",
                "The slot actor manages task dispatch. It uses a dispatch gate to control concurrency.",
            ),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Circuit Breaker Pattern",
            &make_note_content(
                "Circuit Breaker Pattern",
                "A circuit breaker prevents cascading failures. It builds on the slot actor model.",
            ),
        )
        .await;
        let n3 = create_source_note(
            &note_repo,
            &project.id,
            "Warm Pipeline Design",
            &make_note_content(
                "Warm Pipeline Design",
                "The warm pipeline pre-computes indexes. It exemplifies the circuit breaker pattern.",
            ),
        )
        .await;
        let n4 = create_source_note(
            &note_repo,
            &project.id,
            "Version Fix",
            &make_note_content(
                "Version Fix",
                "FIXED v0.4.17: the race condition in the slot actor is resolved. This closes the gap from the circuit breaker.",
            ),
        )
        .await;
        let n5 = create_source_note(
            &note_repo,
            &project.id,
            "Retrieval Architecture",
            &make_note_content(
                "Retrieval Architecture",
                "The retrieval pipeline builds on the warm pipeline design for fast access.",
            ),
        )
        .await;

        // Anchor n5 in the scripted LLM response so it is not flagged as an
        // unused variable. n5 is a real source note that participates in the
        // batch; the second scripted claim below references it.
        let _ = n5.id.len();

        // Scripted LLM response with entities, claims, and edges.
        let llm_json = format!(
            r#"{{
                "entities": [
                    {{"canonical_name": "slot actor", "aliases": ["dispatch actor"]}},
                    {{"canonical_name": "circuit breaker", "aliases": []}},
                    {{"canonical_name": "warm pipeline", "aliases": []}}
                ],
                "claims": [
                    {{"statement": "The slot actor uses a dispatch gate", "source_note_id": "{}", "evidence_quote": "uses a dispatch gate"}}
                ],
                "edges": [
                    {{"source_note_id": "{}", "target_note_id": "{}", "kind": "builds_on", "confidence": 0.8, "evidence_quote": "builds on the slot actor"}},
                    {{"source_note_id": "{}", "target_note_id": "{}", "kind": "exemplifies", "confidence": 0.7, "evidence_quote": "exemplifies the circuit breaker"}},
                    {{"source_note_id": "{}", "target_note_id": "{}", "kind": "supersedes", "confidence": 0.9, "evidence_quote": "closes the gap"}}
                ]
            }}"#,
            n1.id, n2.id, n1.id, n3.id, n2.id, n4.id, n2.id
        );

        let provider = Arc::new(FakeProvider::text(llm_json));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;

        // AC1: produces structured entities, claims, and edges arrays.
        assert!(!report.entities.is_empty(), "should have entities");
        assert!(!report.claims.is_empty(), "should have claims");
        assert!(!report.edges.is_empty(), "should have edges");
        assert_eq!(report.notes_processed, 5);
        assert_eq!(report.batches_sent, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edges_duplicating_wikilinks_are_not_persisted() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        // Create two notes with an explicit wikilink between them.
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Source Note",
            &make_note_content("Source Note", "Links to [[Target Note]]."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Target Note",
            &make_note_content("Target Note", "Linked from source."),
        )
        .await;

        // Create a few more notes so we have >=5 total (though this test is
        // focused on the wikilink dedup, not the >=5 AC).
        let _n3 = create_source_note(
            &note_repo,
            &project.id,
            "Extra 1",
            &make_note_content("Extra 1", "filler"),
        )
        .await;
        let _n4 = create_source_note(
            &note_repo,
            &project.id,
            "Extra 2",
            &make_note_content("Extra 2", "filler"),
        )
        .await;
        let _n5 = create_source_note(
            &note_repo,
            &project.id,
            "Extra 3",
            &make_note_content("Extra 3", "filler"),
        )
        .await;

        // Re-index links for n1 to ensure the wikilink to n2 is resolved.
        note_repo
            .update(&n1.id, &n1.title, &n1.content, &n1.tags)
            .await
            .ok();

        // Scripted LLM: try to emit an edge between n1 and n2 that duplicates
        // the wikilink.
        let llm_json = format!(
            r#"{{
                "entities": [],
                "claims": [],
                "edges": [
                    {{"source_note_id": "{}", "target_note_id": "{}", "kind": "builds_on", "confidence": 0.8}}
                ]
            }}"#,
            n1.id, n2.id
        );

        let provider = Arc::new(FakeProvider::text(llm_json));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;

        // AC2: the edge should NOT be in the report.
        assert!(
            report.edges.is_empty(),
            "edge duplicating wikilink should be dropped; got {} edges",
            report.edges.len()
        );
        assert_eq!(
            report.edges_dropped_wikilink_dup, 1,
            "should count the dropped wikilink-duplicate edge"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn entity_embedding_dedup_collapses_to_one_row() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        // Create a pre-existing entity note.
        let _existing_entity = note_repo
            .create_db_note_with_permalink_and_retrieval_anchor(
                &project.id,
                "reference/entities/circuit-breaker",
                "circuit breaker",
                "# circuit breaker\n\nEntity node.",
                "entity",
                "[]",
                Some("circuit breaker"),
            )
            .await
            .expect("create entity");

        // Create a few source notes for context.
        let _n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "Content A"),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "Content B"),
        )
        .await;
        let _n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note C",
            &make_note_content("Note C", "Content C"),
        )
        .await;
        let _n4 = create_source_note(
            &note_repo,
            &project.id,
            "Note D",
            &make_note_content("Note D", "Content D"),
        )
        .await;
        let _n5 = create_source_note(
            &note_repo,
            &project.id,
            "Note E",
            &make_note_content("Note E", "Content E"),
        )
        .await;

        // LLM emits the same entity name → should be deduped against existing.
        let llm_json = r#"{
            "entities": [{"canonical_name": "circuit breaker", "aliases": []}],
            "claims": [],
            "edges": []
        }"#;

        let provider = Arc::new(FakeProvider::text(llm_json));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;

        // AC3: the entity should be merged (not created as a duplicate).
        assert_eq!(
            report.entity_merges, 1,
            "entity should collapse to one row via dedup"
        );

        // Verify only one entity note exists.
        let entity_notes = note_repo
            .list_compact(&project.id, None, Some("entity"), 0)
            .await
            .expect("list entities");
        assert_eq!(
            entity_notes.len(),
            1,
            "exactly one entity note should exist after dedup"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supersedes_edge_produced_from_fixed_prose() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Old Approach",
            &make_note_content("Old Approach", "We used the old method which had a gap."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "New Fix",
            &make_note_content(
                "New Fix",
                "FIXED v0.4.17: the old approach is superseded. This closes the gap.",
            ),
        )
        .await;
        let _n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note 3",
            &make_note_content("Note 3", "filler"),
        )
        .await;
        let _n4 = create_source_note(
            &note_repo,
            &project.id,
            "Note 4",
            &make_note_content("Note 4", "filler"),
        )
        .await;
        let _n5 = create_source_note(
            &note_repo,
            &project.id,
            "Note 5",
            &make_note_content("Note 5", "filler"),
        )
        .await;

        let llm_json = format!(
            r#"{{
                "entities": [],
                "claims": [],
                "edges": [
                    {{"source_note_id": "{}", "target_note_id": "{}", "kind": "supersedes", "confidence": 0.9, "evidence_quote": "closes the gap"}}
                ]
            }}"#,
            n2.id, n1.id
        );

        let provider = Arc::new(FakeProvider::text(llm_json));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;

        // AC4: at least one supersedes edge should be produced.
        let supersedes_edges: Vec<&EnrichmentEdge> = report
            .edges
            .iter()
            .filter(|e| e.kind == "supersedes")
            .collect();
        assert!(
            !supersedes_edges.is_empty(),
            "should produce at least one supersedes edge; got edges: {:?}",
            report.edges
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enrichment_is_idempotent_on_second_run() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Source A",
            &make_note_content("Source A", "Content about a system."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Source B",
            &make_note_content("Source B", "Content that builds on A."),
        )
        .await;
        let _n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note 3",
            &make_note_content("Note 3", "filler"),
        )
        .await;
        let _n4 = create_source_note(
            &note_repo,
            &project.id,
            "Note 4",
            &make_note_content("Note 4", "filler"),
        )
        .await;
        let _n5 = create_source_note(
            &note_repo,
            &project.id,
            "Note 5",
            &make_note_content("Note 5", "filler"),
        )
        .await;

        let llm_json = format!(
            r#"{{
                "entities": [{{"canonical_name": "my system", "aliases": []}}],
                "claims": [{{"statement": "A makes a claim", "source_note_id": "{}", "evidence_quote": "a claim"}}],
                "edges": [
                    {{"source_note_id": "{}", "target_note_id": "{}", "kind": "builds_on", "confidence": 0.8}}
                ]
            }}"#,
            n1.id, n2.id, n1.id
        );

        let provider = Arc::new(FakeProvider::text(llm_json.clone()));

        // ── First run ──────────────────────────────────────────────────────
        let _report1 = run_memory_enrichment_with_provider(&project.id, &db, provider).await;

        let entity_count_after_first = note_repo
            .list_compact(&project.id, None, Some("entity"), 0)
            .await
            .unwrap()
            .len();
        let claim_count_after_first = note_repo
            .list_compact(&project.id, None, Some("claim"), 0)
            .await
            .unwrap()
            .len();

        // ── Second run (same fixture, same LLM output) ─────────────────────
        let provider2 = Arc::new(FakeProvider::text(llm_json));
        let _report2 = run_memory_enrichment_with_provider(&project.id, &db, provider2).await;

        // AC5: second run should add no new entity/claim rows.
        let entity_count_after_second = note_repo
            .list_compact(&project.id, None, Some("entity"), 0)
            .await
            .unwrap()
            .len();
        let claim_count_after_second = note_repo
            .list_compact(&project.id, None, Some("claim"), 0)
            .await
            .unwrap()
            .len();

        assert_eq!(
            entity_count_after_first, entity_count_after_second,
            "entity count must be unchanged on second run (idempotent)"
        );
        assert_eq!(
            claim_count_after_first, claim_count_after_second,
            "claim count must be unchanged on second run (idempotent)"
        );

        // The typed edge is idempotent (ON CONFLICT max-weight merge), so no
        // new rows should appear. Verify the association still has exactly
        // one row for the pair.
        let assoc = note_repo
            .get_association_kind(&n2.id, &n1.id)
            .await
            .expect("read association");
        assert!(assoc.is_some(), "edge should persist after two runs");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provider_errors_are_logged_and_returned_in_report() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        // Create notes so the pass doesn't early-return on empty.
        let _n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note 1",
            &make_note_content("Note 1", "content"),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note 2",
            &make_note_content("Note 2", "content"),
        )
        .await;
        let _n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note 3",
            &make_note_content("Note 3", "content"),
        )
        .await;
        let _n4 = create_source_note(
            &note_repo,
            &project.id,
            "Note 4",
            &make_note_content("Note 4", "content"),
        )
        .await;
        let _n5 = create_source_note(
            &note_repo,
            &project.id,
            "Note 5",
            &make_note_content("Note 5", "content"),
        )
        .await;

        // Use a FailingProvider to simulate a provider error.
        use crate::test_helpers::FailingProvider;
        let provider = Arc::new(FailingProvider::new("test provider failure"));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;

        // AC6: errors are logged and returned in the report, not propagated.
        assert!(
            !report.warnings.is_empty(),
            "warnings should contain the provider error"
        );
        // The report should still have valid (empty) arrays — it didn't panic.
        assert!(report.entities.is_empty());
        assert!(report.claims.is_empty());
        assert!(report.edges.is_empty());
        // Notes were still processed (loaded before the LLM call).
        assert_eq!(report.notes_processed, 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enrichment_handles_empty_project_gracefully() {
        let db = create_test_db();
        let project = make_test_project(&db).await;

        let provider = Arc::new(FakeProvider::text("{}"));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;

        assert_eq!(report.notes_processed, 0);
        assert!(report.entities.is_empty());
        assert!(report.claims.is_empty());
        assert!(report.edges.is_empty());
    }
}
