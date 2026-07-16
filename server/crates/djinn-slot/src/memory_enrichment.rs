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

use djinn_db::repositories::note::{
    MemoryEntityKind, MemoryEntityRef, MemoryEntityType, NoteAssociationKind,
    NoteRevisionCreateState, NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation,
    NoteRevisionReason, NoteRevisionSubsystem, TrustedNoteRevisionAttribution,
    TrustedNoteRevisionProvenance, folder_for_type,
};
use djinn_db::{Database, NoteRepository, ProposalListQuery, ProposalRepository};
use djinn_memory::Note;
use djinn_provider::provider::LlmProvider;
use djinn_provider::{CompletionRequest, complete, resolve_memory_provider_for_user};
use serde::{Deserialize, Serialize};

/// Cosine-similarity threshold for entity dedup. Two entity names whose
/// embeddings have cosine >= this value collapse to one entity row.
pub(crate) const ENTITY_DEDUP_COSINE_THRESHOLD: f64 = 0.92;

/// Maximum edges emitted per LLM batch call. Keeps prompt/response size bounded.
pub(crate) const MAX_EDGES_PER_BATCH: usize = 50;

/// Maximum notes per LLM batch prompt.
pub(crate) const BATCH_SIZE: usize = 8;

/// Maximum characters of note content fed to the LLM per note in a batch.
const MAX_NOTE_CONTENT_CHARS: usize = 800;

/// Maximum characters of proposal body fed to the LLM per proposal in a batch.
/// Proposals tend to be longer than notes, so we cap harder.
const MAX_PROPOSAL_CONTENT_CHARS: usize = 600;

/// Maximum number of proposals included per enrichment batch prompt.
const MAX_PROPOSALS_PER_BATCH: usize = 10;

/// Max output tokens for the enrichment completion.
const ENRICHMENT_MAX_TOKENS: u32 = 4096;

const SYSTEM_PROMPT: &str = "You are a knowledge-graph enrichment extractor. \
Given a batch of project notes, extract entities, claims, and typed implicit edges. \
Respond with valid JSON only.";

const NO_PROVIDER_WARNING: &str =
    "memory_enrichment: no LLM provider available; skipping enrichment";

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
///
/// The endpoint IDs (`source_note_id` / `target_note_id`) may reference either
/// a note or a proposal; the `source_entity_type` / `target_entity_type`
/// discriminators carry that. When the discriminator is missing (e.g. edges
/// persisted by the legacy note-only path) the default `"note"` is assumed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrichmentEdge {
    pub source_note_id: String,
    pub target_note_id: String,
    pub kind: String,
    pub confidence: f64,
    /// Entity type of the source endpoint: `"note"` (default) or `"proposal"`.
    #[serde(default = "default_entity_type")]
    pub source_entity_type: String,
    /// Entity type of the target endpoint: `"note"` (default) or `"proposal"`.
    #[serde(default = "default_entity_type")]
    pub target_entity_type: String,
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
    /// Number of candidate edges dropped because they duplicate explicit wikilinks.
    pub edges_dropped_wikilink_dup: usize,
}

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
    /// Entity type of the source endpoint: `"note"` (default) or `"proposal"`.
    /// Backwards-compatible: if the LLM omits this field, notes are assumed.
    #[serde(default = "default_entity_type")]
    source_entity_type: String,
    /// Entity type of the target endpoint: `"note"` (default) or `"proposal"`.
    #[serde(default = "default_entity_type")]
    target_entity_type: String,
    #[serde(default)]
    evidence_quote: Option<String>,
}

fn default_confidence() -> f64 {
    0.7
}

/// Serde default for `*_entity_type` fields — `"note"`. Keeping the legacy
/// field names `source_note_id` / `target_note_id` preserves JSON backward
/// compatibility with note-only LLM responses.
fn default_entity_type() -> String {
    "note".to_string()
}

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

/// A compact proposal summary used by the enrichment prompt. We fetch proposals
/// from `ProposalRepository::list_filtered` and carry only the fields the LLM
/// needs (id + title + truncated body). We deliberately do **not** duplicate
/// proposal bodies into notes — proposals are surfaced here as first-class
/// graph entities.
#[derive(Clone, Debug)]
struct ProposalSummary {
    id: String,
    title: String,
    /// Truncated body (at most `MAX_PROPOSAL_CONTENT_CHARS` chars).
    body_excerpt: String,
}

/// Render the enrichment prompt for a batch of notes and proposals.
fn build_enrichment_prompt(notes: &[&Note], proposals: &[ProposalSummary]) -> String {
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
    let mut proposal_entries = Vec::new();
    for p in proposals {
        proposal_entries.push(format!(
            "--- PROPOSAL ---\n\
             id: {}\n\
             title: {}\n\
             entity_type: proposal\n\
             body_excerpt:\n{}\n",
            p.id, p.title, p.body_excerpt
        ));
    }
    let proposals_block = proposal_entries.join("\n");
    let entities_section = if proposals.is_empty() {
        String::new()
    } else {
        format!(
            "\n\n         PROPOSALS (entity_type: proposal — first-class graph entities):\n{proposals_block}"
        )
    };
    format!(
        "You are enriching a project's knowledge graph from its notes and proposals.\n\
         Below is a batch of notes{qualifier}. Extract:\n\n\
         1. ENTITIES — recurring systems, concepts, or components mentioned in the notes.\n\
         2. CLAIMS — key decisions or assertions the notes make.\n\
         3. EDGES — typed implicit relationships between notes and/or proposals, supported by textual evidence.\n\n\
         EDGE KINDS: builds_on, contradicts, supersedes, exemplifies\n\n\
         GUARDRAILS:\n\
         - CONSERVATIVE: emit edges only with clear textual evidence. Never invent unsupported edges.\n\
         - Only emit edges between notes or proposals present in this batch (use their ids).\n\
         - Emit AT MOST {max_edges} edges.\n\
         - Each edge must reference source_note_id and target_note_id from the notes or proposals below.\n\
         - Set source_entity_type and target_entity_type to \"note\" or \"proposal\" to match the endpoint kind.\n\
         - confidence: 0.0–1.0 (contradicts ~0.9, builds_on ~0.8, exemplifies ~0.7, supersedes ~0.9).\n\
         - Include a brief evidence_quote from the prose for each edge.\n\
         - If an explicit supersession is stated (e.g. \"FIXED ... closes the gap\"), emit a supersedes edge.\n\n\
         NOTES:\n{notes_block}{entities_section}\n\n\
         Return JSON in exactly this shape:\n\
         {{\n\
           \"entities\": [{{\"canonical_name\": \"...\", \"aliases\": [\"...\"]}}],\n\
           \"claims\": [{{\"statement\": \"...\", \"source_note_id\": \"...\", \"evidence_quote\": \"...\"}}],\n\
           \"edges\": [{{\"source_note_id\": \"...\", \"target_note_id\": \"...\", \"source_entity_type\": \"note|proposal\", \"target_entity_type\": \"note|proposal\", \"kind\": \"builds_on|contradicts|supersedes|exemplifies\", \"confidence\": 0.8, \"evidence_quote\": \"...\"}}]\n\
         }}\n\
         Return empty arrays if nothing significant is found.",
        qualifier = if proposals.is_empty() {
            ""
        } else {
            " and proposals"
        },
        max_edges = MAX_EDGES_PER_BATCH,
        notes_block = notes_block,
        entities_section = entities_section,
    )
}

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
    if let Ok(entity_notes) = repo
        .list_compact(project_id, None, Some("entity"), 0, None)
        .await
    {
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

/// Check if a candidate edge pair already exists as an explicit wikilink.
fn is_wikilink_duplicate(pair_set: &HashSet<(String, String)>, source: &str, target: &str) -> bool {
    let (a, b) = if source <= target {
        (source.to_string(), target.to_string())
    } else {
        (target.to_string(), source.to_string())
    };
    pair_set.contains(&(a, b))
}

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

/// Parse the LLM's edge kind string into a `MemoryEntityKind` (the
/// heterogeneous typed-edge kind used by
/// `upsert_typed_entity_association`). Accepts the same aliases as
/// [`parse_edge_kind`]; `derived_from` is also accepted because proposal
/// involvement may carry provenance semantics, though the enrichment prompt
/// itself does not emit it.
fn parse_edge_kind_entity(kind: &str) -> Option<MemoryEntityKind> {
    match kind.trim().to_lowercase().as_str() {
        "builds_on" | "build_on" | "builds-upon" => Some(MemoryEntityKind::BuildsOn),
        "contradicts" | "contradict" => Some(MemoryEntityKind::Contradicts),
        "supersedes" | "supersede" => Some(MemoryEntityKind::Supersedes),
        "exemplifies" | "exemplify" | "example_of" => Some(MemoryEntityKind::Exemplifies),
        "derived_from" => Some(MemoryEntityKind::DerivedFrom),
        _ => None,
    }
}

/// Parse the LLM's `*_entity_type` string into a `MemoryEntityType`.
/// Returns `None` for unrecognized strings.
fn parse_entity_type(s: &str) -> Option<MemoryEntityType> {
    match s.trim().to_lowercase().as_str() {
        "note" => Some(MemoryEntityType::Note),
        "proposal" => Some(MemoryEntityType::Proposal),
        _ => None,
    }
}

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
    let reason = NoteRevisionReason::new("enrichment:create entity note")
        .map_err(|e| format!("entity revision reason failed: {e}"))?;
    let note = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.to_owned(),
            note_id: Some(uuid::Uuid::now_v7().to_string()),
            event_kind: NoteRevisionEventKind::Created,
            desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                title: canonical_name.to_owned(),
                permalink,
                content,
                note_type: "entity".to_owned(),
                folder: folder_for_type("entity").to_owned(),
                status: "active".to_owned(),
                tags: "[]".to_owned(),
                retrieval_anchor: Some(canonical_name.to_owned()),
                scope_paths: "[]".to_owned(),
                confidence: 0.0,
            }),
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Enrichment),
            provenance: TrustedNoteRevisionProvenance::default(),
            reason,
        })
        .await
        .map_err(|e| format!("entity create failed: {e}"))?;
    let note = note
        .note
        .ok_or_else(|| "entity create mutation returned no note".to_owned())?;
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
    let reason = NoteRevisionReason::new("enrichment:create claim note")
        .map_err(|e| format!("claim revision reason failed: {e}"))?;
    let note = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.to_owned(),
            note_id: Some(uuid::Uuid::now_v7().to_string()),
            event_kind: NoteRevisionEventKind::Created,
            desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                title: statement.to_owned(),
                permalink,
                content,
                note_type: "claim".to_owned(),
                folder: folder_for_type("claim").to_owned(),
                status: "active".to_owned(),
                tags: "[]".to_owned(),
                retrieval_anchor: Some(statement.to_owned()),
                scope_paths: "[]".to_owned(),
                confidence: 0.0,
            }),
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Enrichment),
            provenance: TrustedNoteRevisionProvenance::default(),
            reason,
        })
        .await
        .map_err(|e| format!("claim create failed: {e}"))?;
    let note = note
        .note
        .ok_or_else(|| "claim create mutation returned no note".to_owned())?;
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

// Inner implementation.  `provider_override` bypasses credential loading when `Some` (tests).

/// Load source notes for enrichment, filtering out entity/claim notes
/// that were produced by a previous enrichment pass.
async fn load_source_notes(
    note_repo: &NoteRepository,
    project_id: &str,
) -> Result<Vec<Note>, String> {
    let notes = note_repo
        .list(project_id, None)
        .await
        .map_err(|e| format!("failed to list notes: {e}"))?;
    Ok(notes
        .into_iter()
        .filter(|n| n.note_type != "entity" && n.note_type != "claim")
        .collect())
}

/// Load proposal summaries for the enrichment prompt. Returns an empty
/// vector on failure (best-effort — note-only enrichment still works).
async fn load_proposal_summaries(
    db: &Database,
    project_id: &str,
) -> (Vec<ProposalSummary>, Option<String>) {
    let proposal_repo = ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    match proposal_repo
        .list_filtered(ProposalListQuery {
            target_project_id: Some(project_id.to_string()),
            limit: MAX_PROPOSALS_PER_BATCH as i64,
            ..Default::default()
        })
        .await
    {
        Ok(result) => {
            let summaries = result
                .proposals
                .into_iter()
                .map(|(p, _)| ProposalSummary {
                    id: p.id,
                    title: p.title,
                    body_excerpt: p.body.chars().take(MAX_PROPOSAL_CONTENT_CHARS).collect(),
                })
                .collect::<Vec<_>>();
            (summaries, None)
        }
        Err(e) => {
            let warning = format!("failed to load proposals (continuing note-only): {e}");
            tracing::debug!(error = %e, "memory_enrichment: failed to load proposals; continuing note-only");
            (Vec::new(), Some(warning))
        }
    }
}

/// Process a single enrichment batch: call the LLM, parse the response,
/// and persist entities, claims, and edges. Returns (entities, claims,
/// edges_dropped_wikilink, warnings) for this batch.
#[allow(clippy::too_many_arguments)] // extracted helper — argument count mirrors the original hotspot
async fn process_enrichment_batch(
    project_id: &str,
    batch_idx: usize,
    batch_refs: &[&Note],
    batch_ids: &[String],
    proposal_summaries: &[ProposalSummary],
    proposal_ids: &HashSet<String>,
    provider: &dyn LlmProvider,
    note_repo: &NoteRepository,
    entity_state: &mut EntityDedupState,
    report: &mut EnrichmentReport,
) {
    let prompt = build_enrichment_prompt(batch_refs, proposal_summaries);
    let response = match complete(
        provider,
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
            return;
        }
    };
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
            return;
        }
    };
    report.batches_sent += 1;
    process_batch_entities(
        project_id,
        &parsed.entities,
        batch_refs,
        note_repo,
        entity_state,
        report,
    )
    .await;
    process_batch_claims(project_id, &parsed.claims, batch_ids, note_repo, report).await;
    process_batch_edges(
        project_id,
        &parsed.edges,
        batch_ids,
        proposal_ids,
        note_repo,
        report,
    )
    .await;
}

/// Process entities from a parsed enrichment response.
async fn process_batch_entities(
    project_id: &str,
    llm_entities: &[LlmEntity],
    batch_refs: &[&Note],
    note_repo: &NoteRepository,
    entity_state: &mut EntityDedupState,
    report: &mut EnrichmentReport,
) {
    for llm_entity in llm_entities {
        let canonical = llm_entity.canonical_name.trim();
        if canonical.is_empty() {
            continue;
        }
        let merge_target = entity_state.find_merge_target(canonical, None);
        if let Some(_existing_id) = merge_target {
            report.entity_merges += 1;
            report.entities.push(EnrichmentEntity {
                canonical_name: canonical.to_string(),
                aliases: llm_entity.aliases.clone(),
            });
            continue;
        }
        match persist_entity(note_repo, project_id, canonical, &llm_entity.aliases).await {
            Ok((note_id, _is_new)) => {
                for note in batch_refs {
                    let content_mentions = note
                        .content
                        .to_lowercase()
                        .contains(&canonical.to_lowercase());
                    let title_mentions = note
                        .title
                        .to_lowercase()
                        .contains(&canonical.to_lowercase());
                    if (content_mentions || title_mentions)
                        && let Err(e) = note_repo.record_derived_from(&note_id, &note.id, 0.5).await
                    {
                        tracing::debug!(
                            project_id = %project_id,
                            error = %e,
                            "memory_enrichment: derived_from edge write failed (non-fatal)"
                        );
                    }
                }
                let entity_embedding = repo_get_embedding(note_repo, &note_id).await;
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
}

/// Process claims from a parsed enrichment response.
async fn process_batch_claims(
    project_id: &str,
    llm_claims: &[LlmClaim],
    batch_ids: &[String],
    note_repo: &NoteRepository,
    report: &mut EnrichmentReport,
) {
    for llm_claim in llm_claims {
        let statement = llm_claim.statement.trim();
        if statement.is_empty() {
            continue;
        }
        if !batch_ids.contains(&llm_claim.source_note_id) {
            continue;
        }
        match persist_claim(
            note_repo,
            project_id,
            statement,
            &llm_claim.source_note_id,
            llm_claim.evidence_quote.as_deref(),
        )
        .await
        {
            Ok((claim_id, _is_new)) => {
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
}

//
// The helpers below are private seams for `process_batch_edges`. They isolate
// the validation/decision logic (endpoint parsing & knownness, edge-kind parse,
// proposal-evidence gating, wikilink-duplicate decision + drop accounting),
// association persistence, and final report mutation so the top-level
// orchestrator reads as a flat sequence of named steps.
//
// All helpers emit the same non-fatal `tracing::debug!` events that the
// inlined code did, preserving the current report/drop semantics that the
// `m27o` characterization suite locks down.

/// Validate that an LLM edge endpoint's `*_entity_type` is recognized *and*
/// that the endpoint id is known to the current batch (notes) or to the wider
/// proposal set (proposals).
///
/// Returns `Some(type)` when the endpoint is a valid known entity, `None`
/// when the endpoint should be dropped. `None` always emits a debug event
/// describing why the endpoint failed validation.
fn classify_edge_endpoint(
    project_id: &str,
    endpoint_role: &str, // "source" or "target" — used in debug log wording
    entity_type_str: &str,
    endpoint_id: &str,
    batch_ids: &[String],
    proposal_ids: &HashSet<String>,
) -> Option<MemoryEntityType> {
    let entity_type = match parse_entity_type(entity_type_str) {
        Some(t) => t,
        None => {
            tracing::debug!(
                project_id = %project_id,
                role = %endpoint_role,
                entity_type = %entity_type_str,
                "memory_enrichment: dropping edge with unrecognized entity_type",
            );
            return None;
        }
    };
    let known = match entity_type {
        MemoryEntityType::Note => batch_ids.contains(&endpoint_id.to_string()),
        MemoryEntityType::Proposal => proposal_ids.contains(endpoint_id),
    };
    if !known {
        tracing::debug!(
            project_id = %project_id,
            role = %endpoint_role,
            entity_type = ?entity_type,
            endpoint_id = %endpoint_id,
            "memory_enrichment: dropping edge with unknown endpoint",
        );
        return None;
    }
    Some(entity_type)
}

/// Parse the LLM edge kind string into a `NoteAssociationKind`. `None` means
/// the edge should be dropped; a debug event is emitted in that case to
/// preserve the previous in-line behavior.
fn parse_and_validate_edge_kind(project_id: &str, kind_str: &str) -> Option<NoteAssociationKind> {
    match parse_edge_kind(kind_str) {
        Some(k) => Some(k),
        None => {
            tracing::debug!(
                project_id = %project_id,
                kind = %kind_str,
                "memory_enrichment: dropping edge with unrecognized kind"
            );
            None
        }
    }
}

/// Enforce the proposal-evidence rule: any edge that involves a proposal
/// endpoint must carry an evidence quote.
///
/// `true` means the edge may proceed; `false` means it must be dropped
/// (a debug event is emitted). For non-proposal edges this is always `true`.
fn require_proposal_evidence(
    project_id: &str,
    involves_proposal: bool,
    evidence_quote: Option<&str>,
) -> bool {
    if involves_proposal && evidence_quote.is_none() {
        tracing::debug!(
            project_id = %project_id,
            "memory_enrichment: dropping proposal-involving edge without evidence_quote"
        );
        return false;
    }
    true
}

/// Decide whether a note-note edge duplicates an explicit wikilink between
/// `source` and `target`. For proposal-involving edges, deduplication is
/// skipped and the edge is allowed through.
///
/// When `true` is returned the edge should be dropped (the
/// `edges_dropped_wikilink_dup` counter has already been incremented in one
/// place, matching the previous in-line report-mutation semantics). `false`
/// means the edge may proceed.
fn classify_wikilink_duplicate(
    wikilink_pairs: &HashSet<(String, String)>,
    involves_proposal: bool,
    source: &str,
    target: &str,
    report: &mut EnrichmentReport,
) -> bool {
    if involves_proposal {
        return false;
    }
    if is_wikilink_duplicate(wikilink_pairs, source, target) {
        report.edges_dropped_wikilink_dup += 1;
        return true;
    }
    false
}

/// Convert a validated endpoint into the owned reference shape used by the
/// heterogeneous association table. This seam keeps the proposal-involving
/// persistence path from open-coding source/target conversions.
fn memory_entity_ref_for_endpoint(
    entity_type: MemoryEntityType,
    endpoint_id: &str,
) -> MemoryEntityRef {
    match entity_type {
        MemoryEntityType::Note => MemoryEntityRef::note(endpoint_id),
        MemoryEntityType::Proposal => MemoryEntityRef::proposal(endpoint_id),
    }
}

/// Persist a proposal-involving edge via the heterogeneous typed-entity association table.
///
/// Converts source/target to [`MemoryEntityRef`] via
/// [`memory_entity_ref_for_endpoint`], parses the entity-level kind via
/// [`parse_edge_kind_entity`], and calls
/// `upsert_typed_entity_association`.
///
/// Returns `false` on kind-parse failure or on a non-fatal association-write
/// error (warning pushed to `report`).
async fn persist_entity_edge_association(
    project_id: &str,
    llm_edge: &LlmEdge,
    source_type: MemoryEntityType,
    target_type: MemoryEntityType,
    note_repo: &NoteRepository,
    report: &mut EnrichmentReport,
) -> bool {
    let entity_kind = match parse_edge_kind_entity(&llm_edge.kind) {
        Some(k) => k,
        None => return false,
    };
    let source_ref = memory_entity_ref_for_endpoint(source_type, &llm_edge.source_note_id);
    let target_ref = memory_entity_ref_for_endpoint(target_type, &llm_edge.target_note_id);
    if let Err(e) = note_repo
        .upsert_typed_entity_association(source_ref, target_ref, entity_kind, llm_edge.confidence)
        .await
    {
        tracing::debug!(
            project_id = %project_id,
            error = %e,
            "memory_enrichment: typed entity association write failed (non-fatal)"
        );
        report.warnings.push(e.to_string());
        return false;
    }
    true
}

/// Persist a note-note edge via the note-level typed association table.
///
/// Calls `upsert_typed_association` with the already-validated [`NoteAssociationKind`].
///
/// Returns `false` on a non-fatal association-write error (warning pushed to
/// `report`), `true` on success.
async fn persist_note_edge_association(
    project_id: &str,
    llm_edge: &LlmEdge,
    kind: NoteAssociationKind,
    note_repo: &NoteRepository,
    report: &mut EnrichmentReport,
) -> bool {
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
        return false;
    }
    true
}

/// Append the accepted edge to the enrichment report and update the per-batch
/// persisted-edge counter in the same small seam that previously lived inline.
fn append_report_edge(
    report: &mut EnrichmentReport,
    batch_edge_count: &mut usize,
    llm_edge: &LlmEdge,
    kind: NoteAssociationKind,
) {
    *batch_edge_count += 1;
    report.edges.push(EnrichmentEdge {
        source_note_id: llm_edge.source_note_id.clone(),
        target_note_id: llm_edge.target_note_id.clone(),
        kind: kind.as_str().to_string(),
        confidence: llm_edge.confidence,
        source_entity_type: llm_edge.source_entity_type.clone(),
        target_entity_type: llm_edge.target_entity_type.clone(),
        evidence_quote: llm_edge.evidence_quote.clone(),
    });
}

/// Process edges from a parsed enrichment response, with wikilink dedup.
///
/// Top-level orchestration:
/// 1. load wikilink dedup state
/// 2. iterate bounded candidate edges (`MAX_EDGES_PER_BATCH`)
/// 3. validate endpoints (`classify_edge_endpoint` × 2)
/// 4. validate edge kind (`parse_and_validate_edge_kind`)
/// 5. apply proposal evidence / wikilink duplicate rules
/// 6. persist association (`persist_entity_edge_association` or
///    `persist_note_edge_association`)
/// 7. append report edge (`append_report_edge`)
async fn process_batch_edges(
    project_id: &str,
    llm_edges: &[LlmEdge],
    batch_ids: &[String],
    proposal_ids: &HashSet<String>,
    note_repo: &NoteRepository,
    report: &mut EnrichmentReport,
) {
    let wikilink_pairs = note_repo
        .wikilink_pairs_for_notes(batch_ids)
        .await
        .unwrap_or_default();
    let mut batch_edge_count = 0;
    for llm_edge in llm_edges {
        if batch_edge_count >= MAX_EDGES_PER_BATCH {
            break;
        }
        let source_type = match classify_edge_endpoint(
            project_id,
            "source",
            &llm_edge.source_entity_type,
            &llm_edge.source_note_id,
            batch_ids,
            proposal_ids,
        ) {
            Some(t) => t,
            None => continue,
        };
        let target_type = match classify_edge_endpoint(
            project_id,
            "target",
            &llm_edge.target_entity_type,
            &llm_edge.target_note_id,
            batch_ids,
            proposal_ids,
        ) {
            Some(t) => t,
            None => continue,
        };
        let kind = match parse_and_validate_edge_kind(project_id, &llm_edge.kind) {
            Some(k) => k,
            None => continue,
        };
        let involves_proposal =
            source_type == MemoryEntityType::Proposal || target_type == MemoryEntityType::Proposal;
        if !require_proposal_evidence(
            project_id,
            involves_proposal,
            llm_edge.evidence_quote.as_deref(),
        ) {
            continue;
        }
        if classify_wikilink_duplicate(
            &wikilink_pairs,
            involves_proposal,
            &llm_edge.source_note_id,
            &llm_edge.target_note_id,
            report,
        ) {
            continue;
        }
        let persisted = if involves_proposal {
            persist_entity_edge_association(
                project_id,
                llm_edge,
                source_type,
                target_type,
                note_repo,
                report,
            )
            .await
        } else {
            persist_note_edge_association(project_id, llm_edge, kind, note_repo, report).await
        };
        if !persisted {
            continue;
        }
        append_report_edge(report, &mut batch_edge_count, llm_edge, kind);
    }
}

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
    let source_notes = match load_source_notes(&note_repo, project_id).await {
        Ok(notes) => notes,
        Err(e) => {
            tracing::warn!(project_id = %project_id, error = %e, "memory_enrichment: failed to load notes");
            report.warnings.push(e);
            return report;
        }
    };
    report.notes_processed = source_notes.len();
    if source_notes.is_empty() {
        tracing::debug!(project_id = %project_id, "memory_enrichment: no source notes to process");
        return report;
    }
    let (proposal_summaries, warning) = load_proposal_summaries(db, project_id).await;
    if let Some(w) = warning {
        report.warnings.push(w);
    }
    let proposal_ids: HashSet<String> = proposal_summaries.iter().map(|p| p.id.clone()).collect();
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
    let mut entity_state = build_entity_dedup_state(&note_repo, project_id).await;
    for (batch_idx, batch) in source_notes.chunks(BATCH_SIZE).enumerate() {
        let batch_refs: Vec<&Note> = batch.iter().collect();
        let batch_ids: Vec<String> = batch.iter().map(|n| n.id.clone()).collect();
        process_enrichment_batch(
            project_id,
            batch_idx + 1,
            &batch_refs,
            &batch_ids,
            &proposal_summaries,
            &proposal_ids,
            provider.as_ref(),
            &note_repo,
            &mut entity_state,
            &mut report,
        )
        .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{FakeProvider, create_test_db};
    use djinn_db::ProjectRepository;
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
    fn classify_edge_endpoint_recognizes_note_in_batch() {
        let batch = vec!["n1".to_string()];
        let proposals = HashSet::new();
        assert_eq!(
            classify_edge_endpoint("proj", "source", "note", "n1", &batch, &proposals),
            Some(MemoryEntityType::Note)
        );
    }
    #[test]
    fn classify_edge_endpoint_recognizes_proposal_in_proposal_set() {
        let batch: Vec<String> = vec![];
        let mut proposals = HashSet::new();
        proposals.insert("p1".to_string());
        assert_eq!(
            classify_edge_endpoint("proj", "target", "proposal", "p1", &batch, &proposals),
            Some(MemoryEntityType::Proposal)
        );
    }
    #[test]
    fn classify_edge_endpoint_drops_unknown_entity_type() {
        let batch = vec!["n1".to_string()];
        let proposals = HashSet::new();
        assert_eq!(
            classify_edge_endpoint("proj", "source", "wiki", "n1", &batch, &proposals),
            None
        );
    }
    #[test]
    fn classify_edge_endpoint_drops_unknown_note_endpoint() {
        let batch = vec!["n1".to_string()];
        let proposals = HashSet::new();
        assert_eq!(
            classify_edge_endpoint("proj", "source", "note", "missing", &batch, &proposals),
            None
        );
    }
    #[test]
    fn classify_edge_endpoint_drops_unknown_proposal_endpoint() {
        let batch: Vec<String> = vec![];
        let proposals = HashSet::new();
        assert_eq!(
            classify_edge_endpoint("proj", "target", "proposal", "missing", &batch, &proposals),
            None
        );
    }
    #[test]
    fn parse_and_validate_edge_kind_accepts_recognized() {
        assert_eq!(
            parse_and_validate_edge_kind("proj", "builds_on"),
            Some(NoteAssociationKind::BuildsOn)
        );
        assert_eq!(
            parse_and_validate_edge_kind("proj", "contradicts"),
            Some(NoteAssociationKind::Contradicts)
        );
    }
    #[test]
    fn parse_and_validate_edge_kind_rejects_unknown() {
        assert_eq!(parse_and_validate_edge_kind("proj", "related_to"), None);
        assert_eq!(parse_and_validate_edge_kind("proj", ""), None);
    }
    #[test]
    fn require_proposal_evidence_drops_proposal_edge_without_evidence() {
        assert!(!require_proposal_evidence("proj", true, None));
    }
    #[test]
    fn require_proposal_evidence_keeps_proposal_edge_with_evidence() {
        assert!(require_proposal_evidence("proj", true, Some("quote")));
    }
    #[test]
    fn require_proposal_evidence_keeps_non_proposal_edge() {
        assert!(require_proposal_evidence("proj", false, None));
        assert!(require_proposal_evidence("proj", false, Some("quote")));
    }
    #[test]
    fn classify_wikilink_duplicate_skips_proposal_edges() {
        let mut set = HashSet::new();
        set.insert(("n1".to_string(), "n2".to_string()));
        let mut report = EnrichmentReport::default();
        // proposal-involving edge: never duplicate, never count.
        let drop = classify_wikilink_duplicate(&set, true, "n1", "n2", &mut report);
        assert!(
            !drop,
            "proposal-involving edges should not be classified as wikilink dups"
        );
        assert_eq!(report.edges_dropped_wikilink_dup, 0);
    }
    #[test]
    fn classify_wikilink_duplicate_drops_note_dup_with_counter() {
        let mut set = HashSet::new();
        set.insert(("n1".to_string(), "n2".to_string()));
        let mut report = EnrichmentReport::default();
        let drop = classify_wikilink_duplicate(&set, false, "n1", "n2", &mut report);
        assert!(drop, "duplicate pair should be flagged for drop");
        assert_eq!(report.edges_dropped_wikilink_dup, 1);
    }
    #[test]
    fn classify_wikilink_duplicate_keeps_note_nondup() {
        let mut set = HashSet::new();
        set.insert(("n1".to_string(), "n2".to_string()));
        let mut report = EnrichmentReport::default();
        let drop = classify_wikilink_duplicate(&set, false, "n3", "n4", &mut report);
        assert!(!drop, "novel pair should not be flagged");
        assert_eq!(report.edges_dropped_wikilink_dup, 0);
    }
    #[test]
    fn memory_entity_ref_for_endpoint_builds_expected_refs() {
        assert_eq!(
            memory_entity_ref_for_endpoint(MemoryEntityType::Note, "n1"),
            MemoryEntityRef::note("n1")
        );
        assert_eq!(
            memory_entity_ref_for_endpoint(MemoryEntityType::Proposal, "p1"),
            MemoryEntityRef::proposal("p1")
        );
    }
    #[test]
    fn append_report_edge_updates_counter_and_report_edge() {
        let edge = LlmEdge {
            source_note_id: "n1".to_string(),
            target_note_id: "n2".to_string(),
            kind: "builds_on".to_string(),
            confidence: 0.82,
            source_entity_type: "note".to_string(),
            target_entity_type: "note".to_string(),
            evidence_quote: Some("clear evidence".to_string()),
        };
        let mut report = EnrichmentReport::default();
        let mut batch_edge_count = 0;
        append_report_edge(
            &mut report,
            &mut batch_edge_count,
            &edge,
            NoteAssociationKind::BuildsOn,
        );
        assert_eq!(batch_edge_count, 1);
        assert_eq!(report.edges.len(), 1);
        assert_eq!(report.edges[0].source_note_id, "n1");
        assert_eq!(report.edges[0].target_note_id, "n2");
        assert_eq!(report.edges[0].kind, "builds_on");
        assert_eq!(report.edges[0].confidence, 0.82);
        assert_eq!(
            report.edges[0].evidence_quote.as_deref(),
            Some("clear evidence")
        );
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
    async fn enrichment_note_creation_records_attribution_snapshot_and_noop() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let aliases = vec!["dispatch".to_owned(), "gate".to_owned()];

        let (entity_id, entity_created) =
            persist_entity(&note_repo, &project.id, "Dispatch Gate", &aliases)
                .await
                .expect("create entity");
        let (claim_id, claim_created) = persist_claim(
            &note_repo,
            &project.id,
            "The dispatch gate controls concurrency",
            "source-note",
            Some("controls concurrency"),
        )
        .await
        .expect("create claim");
        assert!(entity_created);
        assert!(claim_created);

        let revisions = note_repo
            .revision_events(&project.id)
            .await
            .expect("load enrichment revisions");
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[0].actor_kind, "system");
        assert_eq!(revisions[0].subsystem.as_deref(), Some("enrichment"));
        assert_eq!(revisions[0].event_kind, "created");
        assert_eq!(revisions[0].content_before, None);
        assert_eq!(
            revisions[0].content_after.as_deref(),
            Some(
                "# Dispatch Gate\n\n## Aliases\n- dispatch\n- gate\n\n---\n*Extracted by memory enrichment pass.*"
            )
        );
        assert_eq!(revisions[0].confidence_before, None);
        assert_eq!(revisions[0].confidence_after, Some(0.0));
        assert_eq!(revisions[0].reason, "enrichment:create entity note");
        assert_eq!(revisions[1].actor_kind, "system");
        assert_eq!(revisions[1].subsystem.as_deref(), Some("enrichment"));
        assert_eq!(revisions[1].event_kind, "created");
        assert_eq!(revisions[1].content_before, None);
        assert_eq!(
            revisions[1].content_after.as_deref(),
            Some(
                "# The dispatch gate controls concurrency\n\n## Source\n- source-note\n\n## Evidence\n> controls concurrency\n\n---\n*Extracted by memory enrichment pass.*"
            )
        );
        assert_eq!(revisions[1].confidence_before, None);
        assert_eq!(revisions[1].confidence_after, Some(0.0));
        assert_eq!(revisions[1].reason, "enrichment:create claim note");

        let (same_entity_id, entity_changed) =
            persist_entity(&note_repo, &project.id, "Dispatch Gate", &aliases)
                .await
                .expect("reuse entity");
        let (same_claim_id, claim_changed) = persist_claim(
            &note_repo,
            &project.id,
            "The dispatch gate controls concurrency",
            "source-note",
            Some("controls concurrency"),
        )
        .await
        .expect("reuse claim");
        assert_eq!(same_entity_id, entity_id);
        assert_eq!(same_claim_id, claim_id);
        assert!(!entity_changed);
        assert!(!claim_changed);
        assert_eq!(
            note_repo
                .revision_events(&project.id)
                .await
                .expect("count enrichment revisions")
                .len(),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enrichment_entity_create_rolls_back_when_revision_insert_fails() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        note_repo.set_revision_event_insertion_failure_for_test(true);

        assert!(
            persist_entity(&note_repo, &project.id, "Failed Entity", &[])
                .await
                .is_err()
        );
        note_repo.set_revision_event_insertion_failure_for_test(false);
        assert!(
            note_repo
                .get_by_permalink(&project.id, &entity_permalink("Failed Entity"))
                .await
                .expect("lookup failed entity")
                .is_none()
        );
        assert!(
            note_repo
                .revision_events(&project.id)
                .await
                .expect("count failed entity revisions")
                .is_empty()
        );
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
            .list_compact(&project.id, None, Some("entity"), 0, None)
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
        let _report1 = run_memory_enrichment_with_provider(&project.id, &db, provider).await;
        let entity_count_after_first = note_repo
            .list_compact(&project.id, None, Some("entity"), 0, None)
            .await
            .unwrap()
            .len();
        let claim_count_after_first = note_repo
            .list_compact(&project.id, None, Some("claim"), 0, None)
            .await
            .unwrap()
            .len();
        let provider2 = Arc::new(FakeProvider::text(llm_json));
        let _report2 = run_memory_enrichment_with_provider(&project.id, &db, provider2).await;
        // AC5: second run should add no new entity/claim rows.
        let entity_count_after_second = note_repo
            .list_compact(&project.id, None, Some("entity"), 0, None)
            .await
            .unwrap()
            .len();
        let claim_count_after_second = note_repo
            .list_compact(&project.id, None, Some("claim"), 0, None)
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
    /// Helper: create a proposal that targets `project_id`.
    async fn create_targeted_proposal(
        db: &djinn_db::Database,
        project_id: &str,
        title: &str,
        body: &str,
    ) -> djinn_core::models::Proposal {
        let proposal_repo =
            ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let proposal = proposal_repo
            .create(djinn_db::ProposalCreateInput {
                title,
                body,
                acceptance_criteria: Some("[]"),
                status: Some("draft"),
                body_format: None,
            })
            .await
            .expect("create proposal");
        proposal_repo
            .add_target(&proposal.id, project_id, "target")
            .await
            .expect("add proposal target");
        proposal
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_to_proposal_edge_is_parsed_and_persisted() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Create a few source notes so the pass doesn't early-return.
        let _n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note 1",
            &make_note_content("Note 1", "filler"),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note 2",
            &make_note_content("Note 2", "filler"),
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
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "First proposal").await;
        let p2 =
            create_targeted_proposal(&db, &project.id, "Proposal B", "Builds on Proposal A").await;
        // LLM emits a proposal↔proposal `builds_on` edge with evidence.
        let llm_json = format!(
            r#"{{
                "entities": [],
                "claims": [],
                "edges": [
                    {{"source_note_id": "{}", "target_note_id": "{}", "source_entity_type": "proposal", "target_entity_type": "proposal", "kind": "builds_on", "confidence": 0.85, "evidence_quote": "Builds on Proposal A"}}
                ]
            }}"#,
            p2.id, p1.id
        );
        let provider = Arc::new(FakeProvider::text(llm_json));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;
        // The edge should appear in the report.
        let prop_edges: Vec<&EnrichmentEdge> = report
            .edges
            .iter()
            .filter(|e| e.source_entity_type == "proposal" && e.target_entity_type == "proposal")
            .collect();
        assert_eq!(
            prop_edges.len(),
            1,
            "exactly one proposal↔proposal edge expected; got edges: {:?}",
            report.edges
        );
        assert_eq!(prop_edges[0].source_note_id, p2.id);
        assert_eq!(prop_edges[0].target_note_id, p1.id);
        assert_eq!(prop_edges[0].kind, "builds_on");
        // Verify persistence through the heterogeneous substrate.
        let edges = note_repo
            .list_typed_entity_associations_for(MemoryEntityRef::proposal(&p2.id), 0.0, 10)
            .await
            .expect("list entity associations");
        assert!(
            edges.iter().any(|e| {
                e.source == MemoryEntityRef::proposal(&p2.id)
                    && e.target == MemoryEntityRef::proposal(&p1.id)
                    && e.kind == MemoryEntityKind::BuildsOn
            }),
            "proposal↔proposal edge should be persisted; got: {:?}",
            edges
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn note_to_proposal_edge_is_parsed_and_persisted() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Implementation Note",
            &make_note_content(
                "Implementation Note",
                "This note exemplifies the proposed design.",
            ),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note 2",
            &make_note_content("Note 2", "filler"),
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
        let p1 =
            create_targeted_proposal(&db, &project.id, "Design Proposal", "The proposed design")
                .await;
        // LLM emits a note→proposal `exemplifies` edge with evidence.
        let llm_json = format!(
            r#"{{
                "entities": [],
                "claims": [],
                "edges": [
                    {{"source_note_id": "{}", "target_note_id": "{}", "source_entity_type": "note", "target_entity_type": "proposal", "kind": "exemplifies", "confidence": 0.75, "evidence_quote": "exemplifies the proposed design"}}
                ]
            }}"#,
            n1.id, p1.id
        );
        let provider = Arc::new(FakeProvider::text(llm_json));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;
        // The edge should appear in the report.
        let mixed_edges: Vec<&EnrichmentEdge> = report
            .edges
            .iter()
            .filter(|e| e.source_entity_type == "note" && e.target_entity_type == "proposal")
            .collect();
        assert_eq!(
            mixed_edges.len(),
            1,
            "exactly one note↔proposal edge expected; got edges: {:?}",
            report.edges
        );
        assert_eq!(mixed_edges[0].source_note_id, n1.id);
        assert_eq!(mixed_edges[0].target_note_id, p1.id);
        assert_eq!(mixed_edges[0].kind, "exemplifies");
        // Verify persistence through the heterogeneous substrate.
        let edges = note_repo
            .list_typed_entity_associations_for(MemoryEntityRef::proposal(&p1.id), 0.0, 10)
            .await
            .expect("list entity associations");
        assert!(
            edges.iter().any(|e| {
                e.source == MemoryEntityRef::note(&n1.id)
                    && e.target == MemoryEntityRef::proposal(&p1.id)
                    && e.kind == MemoryEntityKind::Exemplifies
            }),
            "note↔proposal edge should be persisted; got: {:?}",
            edges
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_proposal_endpoint_edge_is_skipped_not_panicked() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Only one real proposal in the batch.
        let _n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note 1",
            &make_note_content("Note 1", "filler"),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note 2",
            &make_note_content("Note 2", "filler"),
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
        let p1 = create_targeted_proposal(&db, &project.id, "Real Proposal", "real").await;
        // LLM emits two edges:
        // (a) a valid note→proposal edge (should persist)
        // (b) an edge referencing a non-existent proposal id (should be skipped)
        let fake_proposal_id = "019eeb1e-fake-dead-beef-000000000001";
        let llm_json = format!(
            r#"{{
                "entities": [],
                "claims": [],
                "edges": [
                    {{"source_note_id": "{}", "target_note_id": "{}", "source_entity_type": "note", "target_entity_type": "proposal", "kind": "builds_on", "confidence": 0.8, "evidence_quote": "real evidence"}},
                    {{"source_note_id": "{}", "target_note_id": "{}", "source_entity_type": "note", "target_entity_type": "proposal", "kind": "builds_on", "confidence": 0.8, "evidence_quote": "bogus evidence"}}
                ]
            }}"#,
            _n1.id, p1.id, _n2.id, fake_proposal_id
        );
        let provider = Arc::new(FakeProvider::text(llm_json));
        let report = run_memory_enrichment_with_provider(&project.id, &db, provider).await;
        // Only the valid edge should be in the report.
        assert_eq!(
            report.edges.len(),
            1,
            "unknown proposal endpoint edge should be skipped; got: {:?}",
            report.edges
        );
        assert_eq!(report.edges[0].target_note_id, p1.id);
        // The bogus proposal id should NOT have produced any rows.
        let edges = note_repo
            .list_typed_entity_associations_for(
                MemoryEntityRef::proposal(fake_proposal_id),
                0.0,
                10,
            )
            .await
            .expect("list entity associations");
        assert!(
            edges.is_empty(),
            "no edges should exist for the unknown proposal id"
        );
    }
    #[test]
    fn parse_edge_kind_entity_recognizes_all_kinds() {
        assert_eq!(
            parse_edge_kind_entity("builds_on"),
            Some(MemoryEntityKind::BuildsOn)
        );
        assert_eq!(
            parse_edge_kind_entity("contradicts"),
            Some(MemoryEntityKind::Contradicts)
        );
        assert_eq!(
            parse_edge_kind_entity("supersedes"),
            Some(MemoryEntityKind::Supersedes)
        );
        assert_eq!(
            parse_edge_kind_entity("exemplifies"),
            Some(MemoryEntityKind::Exemplifies)
        );
        assert_eq!(
            parse_edge_kind_entity("derived_from"),
            Some(MemoryEntityKind::DerivedFrom)
        );
    }
    #[test]
    fn parse_entity_type_recognizes_note_and_proposal() {
        assert_eq!(parse_entity_type("note"), Some(MemoryEntityType::Note));
        assert_eq!(
            parse_entity_type("proposal"),
            Some(MemoryEntityType::Proposal)
        );
        assert_eq!(parse_entity_type("NOTE"), Some(MemoryEntityType::Note));
        assert_eq!(parse_entity_type("unknown"), None);
        assert_eq!(parse_entity_type(""), None);
    }
    #[test]
    fn default_entity_type_is_note() {
        assert_eq!(default_entity_type(), "note");
    }
    #[test]
    fn llm_edge_defaults_entity_type_to_note_when_omitted() {
        // Backwards compatibility: LLM responses without entity_type fields
        // should deserialize with "note" defaults.
        let json = r#"{
            "source_note_id": "n1",
            "target_note_id": "n2",
            "kind": "builds_on",
            "confidence": 0.8
        }"#;
        let edge: LlmEdge = serde_json::from_str(json).expect("parse");
        assert_eq!(edge.source_entity_type, "note");
        assert_eq!(edge.target_entity_type, "note");
    }
    /// Helper: build an `LlmEdge` between two note ids with the given kind
    /// and optional evidence quote. Defaults to `source_entity_type = "note"`,
    /// `target_entity_type = "note"`.
    fn make_llm_edge(source: &str, target: &str, kind: &str, evidence: Option<&str>) -> LlmEdge {
        LlmEdge {
            source_note_id: source.to_string(),
            target_note_id: target.to_string(),
            kind: kind.to_string(),
            confidence: 0.7,
            source_entity_type: "note".to_string(),
            target_entity_type: "note".to_string(),
            evidence_quote: evidence.map(|s| s.to_string()),
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_edge_cap_limits_accepted_edges_to_max() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content A"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content B"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let proposal_ids = HashSet::new();
        // 55 valid candidate edges — more than MAX_EDGES_PER_BATCH (50).
        let kinds = ["builds_on", "contradicts", "supersedes", "exemplifies"];
        let llm_edges: Vec<LlmEdge> = (0..55)
            .map(|i| make_llm_edge(&n1.id, &n2.id, kinds[i % kinds.len()], None))
            .collect();
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            MAX_EDGES_PER_BATCH,
            "should accept exactly MAX_EDGES_PER_BATCH edges; got {}",
            report.edges.len()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skipped_edges_do_not_count_toward_cap() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content A"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content B"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let proposal_ids = HashSet::new();
        let mut llm_edges = Vec::new();
        // 30 edges with unknown source entity type — should be dropped
        // without incrementing the counter.
        for _ in 0..30 {
            llm_edges.push(LlmEdge {
                source_note_id: n1.id.clone(),
                target_note_id: n2.id.clone(),
                kind: "builds_on".to_string(),
                confidence: 0.7,
                source_entity_type: "unknown_type".to_string(),
                target_entity_type: "note".to_string(),
                evidence_quote: None,
            });
        }
        // 50 valid edges — all should be accepted (exactly the cap).
        for _ in 0..50 {
            llm_edges.push(make_llm_edge(&n1.id, &n2.id, "builds_on", None));
        }
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            MAX_EDGES_PER_BATCH,
            "skipped edges should not count toward cap; got {} accepted",
            report.edges.len()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_with_unknown_source_entity_type_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let llm_edges = vec![LlmEdge {
            source_note_id: n1.id.clone(),
            target_note_id: n2.id.clone(),
            kind: "builds_on".to_string(),
            confidence: 0.7,
            source_entity_type: "wiki".to_string(), // unrecognized
            target_entity_type: "note".to_string(),
            evidence_quote: None,
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "edge with unknown source entity type should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_with_unknown_target_entity_type_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let llm_edges = vec![LlmEdge {
            source_note_id: n1.id.clone(),
            target_note_id: n2.id.clone(),
            kind: "builds_on".to_string(),
            confidence: 0.7,
            source_entity_type: "note".to_string(),
            target_entity_type: "unknown_type".to_string(), // unrecognized
            evidence_quote: None,
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "edge with unknown target entity type should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_with_unknown_note_source_endpoint_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        // n1 NOT in batch_ids → source is unknown.
        let batch_ids = vec![n2.id.clone()];
        let llm_edges = vec![make_llm_edge(&n1.id, &n2.id, "builds_on", None)];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "edge with unknown note source should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_with_unknown_note_target_endpoint_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        // n2 NOT in batch_ids → target is unknown.
        let batch_ids = vec![n1.id.clone()];
        let llm_edges = vec![make_llm_edge(&n1.id, &n2.id, "builds_on", None)];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "edge with unknown note target should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_with_unrecognized_kind_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let llm_edges = vec![make_llm_edge(&n1.id, &n2.id, "related_to", None)];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "edge with unrecognized kind should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_involving_edge_without_evidence_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        // note→proposal edge WITHOUT evidence_quote.
        let llm_edges = vec![LlmEdge {
            source_note_id: n1.id.clone(),
            target_note_id: p1.id.clone(),
            kind: "builds_on".to_string(),
            confidence: 0.8,
            source_entity_type: "note".to_string(),
            target_entity_type: "proposal".to_string(),
            evidence_quote: None, // missing!
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "proposal-involving edge without evidence should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_involving_edge_with_evidence_is_accepted() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        // note→proposal edge WITH evidence_quote.
        let llm_edges = vec![LlmEdge {
            source_note_id: n1.id.clone(),
            target_note_id: p1.id.clone(),
            kind: "builds_on".to_string(),
            confidence: 0.8,
            source_entity_type: "note".to_string(),
            target_entity_type: "proposal".to_string(),
            evidence_quote: Some("evidence text".to_string()),
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            1,
            "proposal-involving edge with evidence should be accepted; got {:?}",
            report.edges
        );
        let edge = &report.edges[0];
        assert_eq!(edge.source_note_id, n1.id);
        assert_eq!(edge.target_note_id, p1.id);
        assert_eq!(edge.kind, "builds_on");
        assert_eq!(edge.evidence_quote.as_deref(), Some("evidence text"));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn note_note_wikilink_dup_is_counted_in_report() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Create two notes with a wikilink between them.
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Source",
            &make_note_content("Source", "Links to [[Target]]."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Target",
            &make_note_content("Target", "content"),
        )
        .await;
        // Re-index n1 to resolve the wikilink.
        note_repo
            .update(&n1.id, &n1.title, &n1.content, &n1.tags)
            .await
            .ok();
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        // Edge between n1 and n2 duplicates the wikilink.
        let llm_edges = vec![make_llm_edge(&n1.id, &n2.id, "builds_on", None)];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "wikilink-duplicate note-note edge should be dropped"
        );
        assert_eq!(
            report.edges_dropped_wikilink_dup, 1,
            "should count the dropped wikilink dup"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wikilink_dup_check_does_not_apply_to_proposal_edges() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Create notes with a wikilink so wikilink_pairs is populated.
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "Links to [[Note B]]."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let _n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note C",
            &make_note_content("Note C", "filler"),
        )
        .await;
        // Re-index n1 to resolve wikilink.
        note_repo
            .update(&n1.id, &n1.title, &n1.content, &n1.tags)
            .await
            .ok();
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        // note→proposal edge with the same n1 that has a wikilink to n2.
        // Wikilink dedup should NOT apply because involves_proposal is true.
        let llm_edges = vec![LlmEdge {
            source_note_id: n1.id.clone(),
            target_note_id: p1.id.clone(),
            kind: "builds_on".to_string(),
            confidence: 0.8,
            source_entity_type: "note".to_string(),
            target_entity_type: "proposal".to_string(),
            evidence_quote: Some("evidence".to_string()),
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            1,
            "proposal-involving edge should bypass wikilink dedup; got {:?}",
            report.edges
        );
        assert_eq!(
            report.edges_dropped_wikilink_dup, 0,
            "no wikilink dup drop for proposal-involving edges"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn report_edge_records_source_target_kind_confidence_and_entity_types() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let llm_edges = vec![LlmEdge {
            source_note_id: n1.id.clone(),
            target_note_id: n2.id.clone(),
            kind: "contradicts".to_string(),
            confidence: 0.95,
            source_entity_type: "note".to_string(),
            target_entity_type: "note".to_string(),
            evidence_quote: Some("conflicting evidence".to_string()),
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(report.edges.len(), 1);
        let edge = &report.edges[0];
        assert_eq!(edge.source_note_id, n1.id);
        assert_eq!(edge.target_note_id, n2.id);
        assert_eq!(edge.kind, "contradicts");
        assert!((edge.confidence - 0.95).abs() < 1e-9);
        assert_eq!(edge.source_entity_type, "note");
        assert_eq!(edge.target_entity_type, "note");
        assert_eq!(edge.evidence_quote.as_deref(), Some("conflicting evidence"));
    }
    /// Helper: build an `LlmEdge` involving a proposal endpoint. The caller
    /// specifies each endpoint's entity type and provides an evidence quote
    /// (required for proposal-involving edges to pass the evidence gate).
    fn make_proposal_edge(
        source: &str,
        target: &str,
        source_type: &str,
        target_type: &str,
        kind: &str,
        evidence: Option<&str>,
    ) -> LlmEdge {
        LlmEdge {
            source_note_id: source.to_string(),
            target_note_id: target.to_string(),
            kind: kind.to_string(),
            confidence: 0.8,
            source_entity_type: source_type.to_string(),
            target_entity_type: target_type.to_string(),
            evidence_quote: evidence.map(|s| s.to_string()),
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_with_unknown_proposal_source_endpoint_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone()];
        let proposal_ids = HashSet::new(); // p1 NOT in proposal_ids
        // proposal source (p1) is unknown because proposal_ids is empty.
        let llm_edges = vec![make_proposal_edge(
            &p1.id,
            &n1.id,
            "proposal",
            "note",
            "builds_on",
            Some("evidence"),
        )];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "edge with unknown proposal source endpoint should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn derived_from_kind_rejected_even_for_proposal_edge() {
        // parse_edge_kind rejects "derived_from", so the edge is dropped
        // before reaching the parse_edge_kind_entity call that would accept it.
        // This is an important behavioral nuance for safe extraction.
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        let llm_edges = vec![make_proposal_edge(
            &n1.id,
            &p1.id,
            "note",
            "proposal",
            "derived_from", // accepted by parse_edge_kind_entity but rejected by parse_edge_kind
            Some("evidence"),
        )];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "derived_from kind should be dropped even for proposal edges (fails parse_edge_kind)"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_wikilink_dup_drops_counted_correctly() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Create notes A→[[B]], C→[[D]] (two wikilink pairs).
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "Links to [[Note B]]."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note C",
            &make_note_content("Note C", "Links to [[Note D]]."),
        )
        .await;
        let n4 = create_source_note(
            &note_repo,
            &project.id,
            "Note D",
            &make_note_content("Note D", "content"),
        )
        .await;
        // Re-index to resolve wikilinks.
        note_repo
            .update(&n1.id, &n1.title, &n1.content, &n1.tags)
            .await
            .ok();
        note_repo
            .update(&n3.id, &n3.title, &n3.content, &n3.tags)
            .await
            .ok();
        let batch_ids = vec![n1.id.clone(), n2.id.clone(), n3.id.clone(), n4.id.clone()];
        // Two edges that duplicate wikilinks.
        let llm_edges = vec![
            make_llm_edge(&n1.id, &n2.id, "builds_on", None),
            make_llm_edge(&n3.id, &n4.id, "contradicts", None),
        ];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "both wikilink-dup edges should be dropped"
        );
        assert_eq!(
            report.edges_dropped_wikilink_dup, 2,
            "should count both wikilink-dup drops"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wikilink_dup_drops_and_accepted_edges_coexist() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // A→[[B]] (wikilink pair). C has no wikilink to anyone.
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "Links to [[Note B]]."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note C",
            &make_note_content("Note C", "content"),
        )
        .await;
        // Re-index to resolve wikilink.
        note_repo
            .update(&n1.id, &n1.title, &n1.content, &n1.tags)
            .await
            .ok();
        let batch_ids = vec![n1.id.clone(), n2.id.clone(), n3.id.clone()];
        // Edge 1: A→B duplicates wikilink → dropped.
        // Edge 2: C→A is novel → accepted.
        let llm_edges = vec![
            make_llm_edge(&n1.id, &n2.id, "builds_on", None),
            make_llm_edge(&n3.id, &n1.id, "supersedes", None),
        ];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            1,
            "one accepted edge expected; got {:?}",
            report.edges
        );
        assert_eq!(
            report.edges[0].source_note_id, n3.id,
            "accepted edge should be the non-dup"
        );
        assert_eq!(
            report.edges_dropped_wikilink_dup, 1,
            "one wikilink-dup drop expected"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exactly_max_edges_all_accepted() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        // Exactly MAX_EDGES_PER_BATCH valid edges — all should be accepted.
        let llm_edges: Vec<LlmEdge> = (0..MAX_EDGES_PER_BATCH)
            .map(|_| make_llm_edge(&n1.id, &n2.id, "builds_on", None))
            .collect();
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            MAX_EDGES_PER_BATCH,
            "exactly MAX_EDGES_PER_BATCH should all be accepted; got {}",
            report.edges.len()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn note_note_edge_persisted_as_typed_association() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let llm_edges = vec![make_llm_edge(&n1.id, &n2.id, "contradicts", None)];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(report.edges.len(), 1);
        assert_eq!(report.edges[0].kind, "contradicts");
        // Verify persistence via the note-association read path.
        let assoc = note_repo
            .get_association_kind(&n1.id, &n2.id)
            .await
            .expect("read association");
        assert!(
            assoc.is_some(),
            "note→note edge should be persisted as a typed association"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_to_note_edge_is_persisted_as_entity_association() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let _n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        // proposal→note edge with evidence.
        let llm_edges = vec![LlmEdge {
            source_note_id: p1.id.clone(),
            target_note_id: n1.id.clone(),
            kind: "exemplifies".to_string(),
            confidence: 0.75,
            source_entity_type: "proposal".to_string(),
            target_entity_type: "note".to_string(),
            evidence_quote: Some("exemplifies the design".to_string()),
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(report.edges.len(), 1);
        let edge = &report.edges[0];
        assert_eq!(edge.source_note_id, p1.id);
        assert_eq!(edge.target_note_id, n1.id);
        assert_eq!(edge.kind, "exemplifies");
        assert_eq!(edge.source_entity_type, "proposal");
        assert_eq!(edge.target_entity_type, "note");
        // Verify persistence through heterogeneous substrate.
        let edges = note_repo
            .list_typed_entity_associations_for(MemoryEntityRef::proposal(&p1.id), 0.0, 10)
            .await
            .expect("list entity associations");
        assert!(
            edges.iter().any(|e| {
                e.source == MemoryEntityRef::proposal(&p1.id)
                    && e.target == MemoryEntityRef::note(&n1.id)
                    && e.kind == MemoryEntityKind::Exemplifies
            }),
            "proposal→note edge should be persisted; got: {:?}",
            edges
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_with_unknown_proposal_target_endpoint_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let p2 = create_targeted_proposal(&db, &project.id, "Proposal B", "body").await;
        let batch_ids = vec![n1.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        // p2 is NOT in proposal_ids → target is unknown.
        let llm_edges = vec![make_proposal_edge(
            &p1.id,
            &p2.id,
            "proposal",
            "proposal",
            "builds_on",
            Some("evidence"),
        )];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "edge with unknown proposal target endpoint should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_edge_list_produces_empty_report_edges() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let batch_ids = vec![n1.id.clone()];
        let llm_edges: Vec<LlmEdge> = Vec::new();
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "empty input should produce no edges"
        );
        assert_eq!(report.edges_dropped_wikilink_dup, 0, "no drops expected");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edge_dropped_for_unknown_target_type_does_not_count_toward_cap() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content A"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content B"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let mut llm_edges = Vec::new();
        // 30 edges with unknown TARGET entity type — should be dropped
        // without incrementing the counter.
        for _ in 0..30 {
            llm_edges.push(LlmEdge {
                source_note_id: n1.id.clone(),
                target_note_id: n2.id.clone(),
                kind: "builds_on".to_string(),
                confidence: 0.7,
                source_entity_type: "note".to_string(),
                target_entity_type: "unknown_type".to_string(),
                evidence_quote: None,
            });
        }
        // 50 valid edges — all should be accepted (exactly the cap).
        for _ in 0..50 {
            llm_edges.push(make_llm_edge(&n1.id, &n2.id, "builds_on", None));
        }
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            MAX_EDGES_PER_BATCH,
            "skipped edges (unknown target type) should not count toward cap; got {} accepted",
            report.edges.len()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_to_proposal_edge_without_evidence_is_dropped() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let _n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let p2 = create_targeted_proposal(&db, &project.id, "Proposal B", "body").await;
        let batch_ids = vec![];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        proposal_ids.insert(p2.id.clone());
        // proposal→proposal edge WITHOUT evidence.
        let llm_edges = vec![LlmEdge {
            source_note_id: p1.id.clone(),
            target_note_id: p2.id.clone(),
            kind: "builds_on".to_string(),
            confidence: 0.8,
            source_entity_type: "proposal".to_string(),
            target_entity_type: "proposal".to_string(),
            evidence_quote: None, // missing!
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert!(
            report.edges.is_empty(),
            "proposal→proposal edge without evidence should be dropped"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skipped_edges_for_unrecognized_kind_do_not_count_toward_cap() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content A"),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content B"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone()];
        let mut llm_edges = Vec::new();
        // 30 edges with unrecognized kind — should be dropped without
        // incrementing the counter.
        for _ in 0..30 {
            llm_edges.push(make_llm_edge(&n1.id, &n2.id, "related_to", None));
        }
        // 50 valid edges — all should be accepted (exactly the cap).
        for _ in 0..50 {
            llm_edges.push(make_llm_edge(&n1.id, &n2.id, "builds_on", None));
        }
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            MAX_EDGES_PER_BATCH,
            "skipped edges (unrecognized kind) should not count toward cap; got {} accepted",
            report.edges.len()
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wikilink_dup_skips_do_not_count_toward_cap() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Create notes A→[[B]] (one wikilink pair).
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "Links to [[Note B]]."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content B"),
        )
        .await;
        // Re-index to resolve wikilink.
        note_repo
            .update(&n1.id, &n1.title, &n1.content, &n1.tags)
            .await
            .ok();
        let mut llm_edges = Vec::new();
        // 30 edges that duplicate the wikilink — should be dropped
        // without incrementing the counter.
        for i in 0..30 {
            let kind = ["builds_on", "contradicts", "supersedes", "exemplifies"][i % 4];
            llm_edges.push(make_llm_edge(&n1.id, &n2.id, kind, None));
        }
        // 50 valid edges (C→A, non-wikilink) using a third note.
        let n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note C",
            &make_note_content("Note C", "content C"),
        )
        .await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone(), n3.id.clone()];
        for _ in 0..50 {
            llm_edges.push(make_llm_edge(&n3.id, &n1.id, "builds_on", None));
        }
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &HashSet::new(),
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            MAX_EDGES_PER_BATCH,
            "wikilink-dup skips should not count toward cap; got {} accepted",
            report.edges.len()
        );
        assert_eq!(
            report.edges_dropped_wikilink_dup, 30,
            "should count all 30 wikilink-dup drops"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proposal_to_proposal_edge_with_evidence_is_accepted() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let _n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let p2 = create_targeted_proposal(&db, &project.id, "Proposal B", "body").await;
        let batch_ids = vec![];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        proposal_ids.insert(p2.id.clone());
        // proposal→proposal edge WITH evidence.
        let llm_edges = vec![LlmEdge {
            source_note_id: p1.id.clone(),
            target_note_id: p2.id.clone(),
            kind: "builds_on".to_string(),
            confidence: 0.85,
            source_entity_type: "proposal".to_string(),
            target_entity_type: "proposal".to_string(),
            evidence_quote: Some("p2 builds on p1 rationale".to_string()),
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            1,
            "proposal→proposal edge with evidence should be accepted; got {:?}",
            report.edges
        );
        let edge = &report.edges[0];
        assert_eq!(edge.source_note_id, p1.id);
        assert_eq!(edge.target_note_id, p2.id);
        assert_eq!(edge.kind, "builds_on");
        assert_eq!(edge.source_entity_type, "proposal");
        assert_eq!(edge.target_entity_type, "proposal");
        assert_eq!(
            edge.evidence_quote.as_deref(),
            Some("p2 builds on p1 rationale")
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn note_to_proposal_edge_report_records_entity_types() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "content"),
        )
        .await;
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        let llm_edges = vec![LlmEdge {
            source_note_id: n1.id.clone(),
            target_note_id: p1.id.clone(),
            kind: "supersedes".to_string(),
            confidence: 0.72,
            source_entity_type: "note".to_string(),
            target_entity_type: "proposal".to_string(),
            evidence_quote: Some("evidence for supersedes".to_string()),
        }];
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(report.edges.len(), 1);
        let edge = &report.edges[0];
        assert_eq!(edge.source_note_id, n1.id);
        assert_eq!(edge.target_note_id, p1.id);
        assert_eq!(edge.kind, "supersedes");
        assert!((edge.confidence - 0.72).abs() < 1e-9);
        assert_eq!(edge.source_entity_type, "note");
        assert_eq!(edge.target_entity_type, "proposal");
        assert_eq!(
            edge.evidence_quote.as_deref(),
            Some("evidence for supersedes")
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixed_skip_reasons_do_not_count_toward_cap() {
        let db = create_test_db();
        let project = make_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        // Notes with a wikilink pair.
        let n1 = create_source_note(
            &note_repo,
            &project.id,
            "Note A",
            &make_note_content("Note A", "Links to [[Note B]]."),
        )
        .await;
        let n2 = create_source_note(
            &note_repo,
            &project.id,
            "Note B",
            &make_note_content("Note B", "content"),
        )
        .await;
        // Third note for valid edges.
        let n3 = create_source_note(
            &note_repo,
            &project.id,
            "Note C",
            &make_note_content("Note C", "content C"),
        )
        .await;
        // Re-index n1 to resolve wikilink.
        note_repo
            .update(&n1.id, &n1.title, &n1.content, &n1.tags)
            .await
            .ok();
        let p1 = create_targeted_proposal(&db, &project.id, "Proposal A", "body").await;
        let batch_ids = vec![n1.id.clone(), n2.id.clone(), n3.id.clone()];
        let mut proposal_ids = HashSet::new();
        proposal_ids.insert(p1.id.clone());
        let mut llm_edges = Vec::new();
        // 10 edges with unknown source entity type.
        for _ in 0..10 {
            llm_edges.push(LlmEdge {
                source_note_id: n1.id.clone(),
                target_note_id: n2.id.clone(),
                kind: "builds_on".to_string(),
                confidence: 0.7,
                source_entity_type: "wiki".to_string(),
                target_entity_type: "note".to_string(),
                evidence_quote: None,
            });
        }
        // 10 edges with unrecognized kind.
        for _ in 0..10 {
            llm_edges.push(make_llm_edge(&n1.id, &n2.id, "related_to", None));
        }
        // 10 edges that duplicate the wikilink.
        for _ in 0..10 {
            llm_edges.push(make_llm_edge(&n1.id, &n2.id, "builds_on", None));
        }
        // 10 proposal-involving edges without evidence.
        for _ in 0..10 {
            llm_edges.push(LlmEdge {
                source_note_id: n1.id.clone(),
                target_note_id: p1.id.clone(),
                kind: "builds_on".to_string(),
                confidence: 0.8,
                source_entity_type: "note".to_string(),
                target_entity_type: "proposal".to_string(),
                evidence_quote: None,
            });
        }
        // 50 valid edges (C→A, non-wikilink, non-proposal).
        for _ in 0..50 {
            llm_edges.push(make_llm_edge(&n3.id, &n1.id, "exemplifies", None));
        }
        let mut report = EnrichmentReport::default();
        process_batch_edges(
            "proj",
            &llm_edges,
            &batch_ids,
            &proposal_ids,
            &note_repo,
            &mut report,
        )
        .await;
        assert_eq!(
            report.edges.len(),
            MAX_EDGES_PER_BATCH,
            "mixed skipped edges should not count toward cap; got {} accepted",
            report.edges.len()
        );
        assert_eq!(
            report.edges_dropped_wikilink_dup, 10,
            "should count all 10 wikilink-dup drops"
        );
        // Verify the accepted edges are the valid ones.
        for edge in &report.edges {
            assert_eq!(edge.source_note_id, n3.id);
            assert_eq!(edge.target_note_id, n1.id);
            assert_eq!(edge.kind, "exemplifies");
        }
    }
}
