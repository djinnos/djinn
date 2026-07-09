//! Real Postgres fixture loader for the Phase 1 memory-eval benchmark.
//!
//! Loads committed JSONL fixtures into an isolated Postgres test/eval database
//! using production `NoteRepository` utilities rather than a mock scorer.
//!
//! The loader validates fixture data before running: notes, lifecycle
//! timestamps/status, labels/entities, graph edges, embedding hashes/vectors,
//! and task-affinity/memory_refs rows. Missing fixture data for any claimed
//! `expected_signal_coverage` is a hard error.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use djinn_core::models::Project;
use djinn_db::database::Database;
use djinn_db::repositories::note::{NoteRepository, UpsertNoteEmbedding};
use djinn_db::repositories::test_support::make_project;

use crate::fixtures::{
    self, BadCaseRow, CorpusNoteRow, EmbeddingRef, FixtureManifest, FixturePaths,
    MinedMemoryRefRow, Phase1Fixtures,
};

// ── Loaded database state ─────────────────────────────────────────────────

/// The state of the loaded fixture database: the project, note IDs keyed by
/// permalink, and the task IDs that were set up for task-affinity scoring.
#[derive(Clone, Debug)]
pub struct LoadedFixtureState {
    /// The isolated eval project.
    pub project: Project,
    /// Map from fixture permalink to the database-generated note ID.
    pub note_id_by_permalink: HashMap<String, String>,
    /// Task IDs created for memory-ref queries that have `task_id` set.
    /// Maps fixture `task_id` string → database task UUID.
    pub task_id_map: HashMap<String, String>,
    /// Epic ID used for all task rows (required FK).
    #[allow(dead_code)]
    pub epic_id: String,
}

fn task_memory_ref_note_ids(
    fixtures: &Phase1Fixtures,
    fixture_task_id: &str,
    note_id_by_permalink: &HashMap<String, String>,
) -> Vec<String> {
    let mut memory_ref_permalinks: BTreeSet<&str> = BTreeSet::new();

    for query in &fixtures.memory_ref_queries {
        if query.task_id.as_deref() == Some(fixture_task_id) {
            memory_ref_permalinks.extend(query.memory_refs.iter().map(String::as_str));
        }
    }

    for case in &fixtures.bad_cases {
        if case.expected_signals.task_affinity && case.task_id.as_deref() == Some(fixture_task_id) {
            memory_ref_permalinks.extend(case.relevant_note_permalinks.iter().map(String::as_str));
        }
    }

    memory_ref_permalinks
        .into_iter()
        .filter_map(|permalink| note_id_by_permalink.get(permalink).cloned())
        .collect()
}

// ── Fixture validation ────────────────────────────────────────────────────

/// Validate that the fixture set is internally consistent and that all data
/// required by claimed retrieval signals is present. Returns hard errors
/// (not warnings) for missing data.
pub fn validate_fixtures(fixtures: &Phase1Fixtures) -> Result<()> {
    let mut errors = Vec::new();

    // 1. Cross-reference validation (permalinks, graph edges)
    let ref_errors = fixtures.validate_references();
    errors.extend(ref_errors);

    // 2. Per-note validation
    for note in &fixtures.corpus_notes {
        // Lifecycle timestamps must be non-empty
        if note.timestamps.created_at.is_empty() {
            errors.push(format!("note '{}': created_at is empty", note.permalink));
        }
        if note.timestamps.updated_at.is_empty() {
            errors.push(format!("note '{}': updated_at is empty", note.permalink));
        }
        if note.timestamps.last_accessed.is_empty() {
            errors.push(format!("note '{}': last_accessed is empty", note.permalink));
        }

        // Status must be valid
        if !["active", "archived", "deprecated"].contains(&note.status.as_str()) {
            errors.push(format!(
                "note '{}': invalid status '{}'",
                note.permalink, note.status
            ));
        }

        // If graph signal is claimed, graph edges must be non-empty
        if note.expected_signals.graph && note.graph_edges.is_empty() {
            errors.push(format!(
                "note '{}': graph signal claimed but no graph_edges provided",
                note.permalink
            ));
        }

        // If entity signal is claimed, labels must be non-empty
        if note.expected_signals.entity && note.labels.is_empty() {
            errors.push(format!(
                "note '{}': entity signal claimed but no labels provided",
                note.permalink
            ));
        }

        // If vector signal is claimed, embedding must be present
        if note.expected_signals.vector && note.embedding.is_none() {
            errors.push(format!(
                "note '{}': vector signal claimed but no embedding provided",
                note.permalink
            ));
        }

        // Validate embedding data integrity when present
        if let Some(ref emb) = note.embedding {
            validate_embedding(&note.permalink, emb, &mut errors);
        }
    }

    // 3. Per-query validation
    for query in &fixtures.memory_ref_queries {
        if query.query_text.is_empty() {
            errors.push(format!("query '{}': query_text is empty", query.query_id));
        }
        if query.memory_refs.is_empty() {
            errors.push(format!(
                "query '{}': memory_refs is empty (no ground truth)",
                query.query_id
            ));
        }
        // If task_affinity signal is claimed, task_id must be present
        if query.expected_signals.task_affinity && query.task_id.is_none() {
            errors.push(format!(
                "query '{}': task_affinity signal claimed but no task_id provided",
                query.query_id
            ));
        }
    }

    // 4. Per-bad-case validation
    for case in &fixtures.bad_cases {
        if case.query_text.is_empty() {
            errors.push(format!("bad-case '{}': query_text is empty", case.case_id));
        }
        if case.relevant_note_permalinks.is_empty() {
            errors.push(format!(
                "bad-case '{}': no relevant_note_permalinks (no ground truth)",
                case.case_id
            ));
        }
        // If task_affinity signal is claimed, task_id must be present
        if case.expected_signals.task_affinity && case.task_id.is_none() {
            errors.push(format!(
                "bad-case '{}': task_affinity signal claimed but no task_id provided",
                case.case_id
            ));
        }
    }

    // 5. Manifest validation (if present)
    if let Some(ref manifest) = fixtures.manifest {
        if manifest.corpus_note_count != fixtures.corpus_notes.len() {
            errors.push(format!(
                "manifest corpus_note_count {} != actual {}",
                manifest.corpus_note_count,
                fixtures.corpus_notes.len()
            ));
        }
        if manifest.memory_ref_query_count != fixtures.memory_ref_queries.len() {
            errors.push(format!(
                "manifest memory_ref_query_count {} != actual {}",
                manifest.memory_ref_query_count,
                fixtures.memory_ref_queries.len()
            ));
        }
        if manifest.bad_case_count != fixtures.bad_cases.len() {
            errors.push(format!(
                "manifest bad_case_count {} != actual {}",
                manifest.bad_case_count,
                fixtures.bad_cases.len()
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "Fixture validation failed with {} error(s):\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        );
    }
}

fn validate_embedding(permalink: &str, emb: &EmbeddingRef, errors: &mut Vec<String>) {
    if emb.content_hash.is_empty() {
        errors.push(format!(
            "note '{}': embedding content_hash is empty",
            permalink
        ));
    }
    if emb.model_version.is_empty() {
        errors.push(format!(
            "note '{}': embedding model_version is empty",
            permalink
        ));
    }
    if emb.vector.is_empty() {
        errors.push(format!("note '{}': embedding vector is empty", permalink));
    }
    if emb.embedding_dim != emb.vector.len() {
        errors.push(format!(
            "note '{}': embedding_dim {} != vector length {}",
            permalink,
            emb.embedding_dim,
            emb.vector.len()
        ));
    }
}

// ── Fixture loading ───────────────────────────────────────────────────────

/// Load the Phase 1 fixture set into an isolated Postgres test database.
///
/// Returns the loaded state with note ID mappings and task IDs for downstream
/// search/build_context execution.
pub async fn load_fixtures(db: &Database, fixtures: &Phase1Fixtures) -> Result<LoadedFixtureState> {
    db.ensure_initialized()
        .await
        .context("ensuring database is initialized")?;

    // Create an eval project
    let project = make_project(db, Path::new("memory-eval")).await;
    info!(project_id = %project.id, "created eval project");

    // Create an epic for task rows (required FK)
    let epic_id = create_epic(db, &project.id, "memory-eval-epic").await;
    info!(epic_id = %epic_id, "created eval epic");

    // ── Phase 1: Insert notes ─────────────────────────────────────────────
    let repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let mut note_id_by_permalink: HashMap<String, String> = HashMap::new();

    for note_row in &fixtures.corpus_notes {
        let note = insert_corpus_note(db, &project.id, note_row)
            .await
            .with_context(|| format!("inserting note '{}'", note_row.permalink))?;
        note_id_by_permalink.insert(note_row.permalink.clone(), note.id.clone());
    }
    info!(count = note_id_by_permalink.len(), "inserted corpus notes");

    // ── Phase 2: Insert embeddings ────────────────────────────────────────
    let mut embedding_count = 0usize;
    for note_row in &fixtures.corpus_notes {
        if let (Some(emb), Some(note_id)) = (
            note_row.embedding.as_ref(),
            note_id_by_permalink.get(&note_row.permalink),
        ) {
            repo.upsert_embedding(UpsertNoteEmbedding {
                note_id,
                content_hash: &emb.content_hash,
                model_version: &emb.model_version,
                embedding: &emb.vector,
                branch: "main",
            })
            .await
            .with_context(|| format!("upserting embedding for '{}'", note_row.permalink))?;
            embedding_count += 1;
        }
    }
    info!(count = embedding_count, "inserted embeddings");

    // ── Phase 3: Insert graph edges as note_associations ──────────────────
    // Collect all edges globally (deduped by source+target+kind)
    let mut edge_set: HashSet<(String, String, String)> = HashSet::new();
    for note_row in &fixtures.corpus_notes {
        for edge in &note_row.graph_edges {
            let key = (
                edge.source_permalink.clone(),
                edge.target_permalink.clone(),
                edge.kind.clone(),
            );
            edge_set.insert(key);
        }
    }

    let mut graph_edge_count = 0usize;
    for (src_permalink, tgt_permalink, kind) in &edge_set {
        let src_id = note_id_by_permalink.get(src_permalink);
        let tgt_id = note_id_by_permalink.get(tgt_permalink);
        if let (Some(src_id), Some(tgt_id)) = (src_id, tgt_id) {
            // For graph proximity scoring, we need to use note_associations.
            // Use upsert_typed_association for typed edges.
            if let Some(assoc_kind) = parse_note_association_kind(kind) {
                repo.upsert_typed_association(src_id, tgt_id, assoc_kind, 1.0)
                    .await
                    .with_context(|| {
                        format!(
                            "inserting graph edge {} -> {} ({})",
                            src_permalink, tgt_permalink, kind
                        )
                    })?;
                graph_edge_count += 1;
            } else if kind == "wikilink" {
                // Wikilinks go through co_access association
                repo.upsert_association(src_id, tgt_id, 1)
                    .await
                    .with_context(|| {
                        format!(
                            "inserting wikilink edge {} -> {}",
                            src_permalink, tgt_permalink
                        )
                    })?;
                graph_edge_count += 1;
            } else {
                warn!(
                    src = %src_permalink,
                    tgt = %tgt_permalink,
                    kind = %kind,
                    "unknown graph edge kind; skipping"
                );
            }
        }
    }
    info!(count = graph_edge_count, "inserted graph edges");

    // ── Phase 4: Insert entity/label associations ─────────────────────────
    // Labels are stored as memory_entity_associations with kind derived_from
    // or as entity rows. For the entity signal, we need to ensure that the
    // entity-overlap boosting path sees the labels. Labels on notes are
    // typically stored in the `notes` table content or as entity associations.
    // For the eval, we insert entity associations between notes that share
    // entity names.
    let mut entity_count = 0usize;

    // Build a map: entity (type, name) → list of note permalinks
    let mut entity_to_notes: HashMap<(String, String), Vec<String>> = HashMap::new();
    for note_row in &fixtures.corpus_notes {
        for label in &note_row.labels {
            let key = (label.entity_type.clone(), label.name.clone());
            entity_to_notes
                .entry(key)
                .or_default()
                .push(note_row.permalink.clone());
        }
    }

    // For entities shared by multiple notes, create typed associations
    for ((_entity_type, _entity_name), note_permalinks) in &entity_to_notes {
        if note_permalinks.len() > 1 {
            // Notes sharing an entity get an exemplifies association
            for i in 0..note_permalinks.len() {
                for j in (i + 1)..note_permalinks.len() {
                    let id_a = note_id_by_permalink.get(&note_permalinks[i]);
                    let id_b = note_id_by_permalink.get(&note_permalinks[j]);
                    if let (Some(id_a), Some(id_b)) = (id_a, id_b) {
                        repo.upsert_typed_association(
                            id_a,
                            id_b,
                            djinn_db::repositories::note::NoteAssociationKind::Exemplifies,
                            0.8,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "inserting entity association {} <-> {}",
                                note_permalinks[i], note_permalinks[j]
                            )
                        })?;
                        entity_count += 1;
                    }
                }
            }
        }
    }
    info!(count = entity_count, "inserted entity associations");

    // ── Phase 5: Create task rows for task-affinity ───────────────────────
    let mut task_id_map: HashMap<String, String> = HashMap::new();

    // Collect all unique task_ids from queries and bad cases. Keep ordering
    // deterministic so seeded task rows and their memory_refs are byte-stable
    // across repeated eval runs.
    let mut all_task_ids: BTreeSet<String> = BTreeSet::new();
    for query in &fixtures.memory_ref_queries {
        if let Some(ref task_id) = query.task_id {
            all_task_ids.insert(task_id.clone());
        }
    }
    for case in &fixtures.bad_cases {
        if let Some(ref task_id) = case.task_id {
            all_task_ids.insert(task_id.clone());
        }
    }

    for fixture_task_id in &all_task_ids {
        // Determine which notes this task references via memory_refs. Task
        // affinity is claimed by both mined memory-ref query rows and bad-case
        // rows; bad-case-only task-affinity fixtures must seed the task refs
        // too, otherwise the eval can claim coverage while creating an empty
        // `tasks.memory_refs` array.
        let memory_refs =
            task_memory_ref_note_ids(fixtures, fixture_task_id, &note_id_by_permalink);

        // Create the task with memory_refs pointing to note IDs
        let db_task_id =
            create_task_with_memory_refs(db, &project.id, &epic_id, fixture_task_id, &memory_refs)
                .await
                .with_context(|| format!("creating task '{}'", fixture_task_id))?;

        task_id_map.insert(fixture_task_id.clone(), db_task_id);
    }
    info!(count = task_id_map.len(), "created task-affinity tasks");

    Ok(LoadedFixtureState {
        project,
        note_id_by_permalink,
        task_id_map,
        epic_id,
    })
}

// ── Internal helpers ──────────────────────────────────────────────────────

/// Insert a corpus note row using the djinn-db test_support seed helper,
/// which provides full control over timestamps, status, and confidence
/// without triggering wikilink indexing or event emission.
async fn insert_corpus_note(
    db: &Database,
    project_id: &str,
    row: &CorpusNoteRow,
) -> Result<djinn_memory::Note> {
    let id = uuid::Uuid::now_v7().to_string();
    let folder = if row.folder.is_empty() {
        djinn_db::repositories::note::folder_for_type(&row.note_type)
    } else {
        &row.folder
    };
    let tags_json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&row.tags)?).unwrap_or(serde_json::json!([]));
    let tags_str = serde_json::to_string(&row.tags)?;
    let content_hash = djinn_db::repositories::note::embedding_content_hash(
        &row.title,
        &row.note_type,
        &tags_str,
        &row.content,
        row.retrieval_anchor.as_deref(),
    );

    let note = djinn_db::repositories::test_support::seed_eval_note(
        db,
        &id,
        project_id,
        &row.permalink,
        &row.title,
        &row.note_type,
        folder,
        &tags_json,
        &row.content,
        row.retrieval_anchor.as_deref(),
        &content_hash,
        &row.timestamps.created_at,
        &row.timestamps.updated_at,
        &row.timestamps.last_accessed,
        &row.status,
        row.confidence,
    )
    .await;

    Ok(note)
}

/// Parse a fixture edge kind string into a `NoteAssociationKind`.
fn parse_note_association_kind(
    kind: &str,
) -> Option<djinn_db::repositories::note::NoteAssociationKind> {
    use djinn_db::repositories::note::NoteAssociationKind;
    match kind {
        "builds_on" => Some(NoteAssociationKind::BuildsOn),
        "contradicts" => Some(NoteAssociationKind::Contradicts),
        "supersedes" => Some(NoteAssociationKind::Supersedes),
        "exemplifies" => Some(NoteAssociationKind::Exemplifies),
        "derived_from" => Some(NoteAssociationKind::DerivedFrom),
        "authored" => Some(NoteAssociationKind::Authored),
        "embedding_related" => Some(NoteAssociationKind::EmbeddingRelated),
        "co_access" => Some(NoteAssociationKind::CoAccess),
        _ => None,
    }
}

/// Create an epic row using the djinn-db test_support seed helper.
async fn create_epic(db: &Database, project_id: &str, title: &str) -> String {
    djinn_db::repositories::test_support::seed_eval_epic(db, project_id, title).await
}

/// Create a task with memory_refs pointing to note IDs (for task-affinity scoring).
async fn create_task_with_memory_refs(
    db: &Database,
    project_id: &str,
    epic_id: &str,
    fixture_task_id: &str,
    memory_refs_note_ids: &[String],
) -> Result<String> {
    let memory_refs_json = serde_json::to_string(memory_refs_note_ids)?;
    let task_id = djinn_db::repositories::test_support::seed_eval_task_with_memory_refs(
        db,
        project_id,
        epic_id,
        fixture_task_id,
        &memory_refs_json,
    )
    .await;
    Ok(task_id)
}

// ── Fixture file loading ──────────────────────────────────────────────────

/// Load fixtures from the crate's fixture directory. Resolves paths relative
/// to the crate root.
pub fn load_fixtures_from_disk(crate_root: &Path) -> Result<Phase1Fixtures> {
    let corpus_path = crate_root.join(FixturePaths::CORPUS_NOTES);
    let queries_path = crate_root.join(FixturePaths::MEMORY_REF_QUERIES);
    let bad_cases_path = crate_root.join(FixturePaths::BAD_CASES);
    let manifest_path = crate_root.join(FixturePaths::MANIFEST);

    let corpus_json = std::fs::read_to_string(&corpus_path)
        .with_context(|| format!("reading {}", corpus_path.display()))?;
    let queries_json = std::fs::read_to_string(&queries_path)
        .with_context(|| format!("reading {}", queries_path.display()))?;
    let bad_cases_json = std::fs::read_to_string(&bad_cases_path)
        .with_context(|| format!("reading {}", bad_cases_path.display()))?;

    let corpus_notes: Vec<CorpusNoteRow> = fixtures::parse_jsonl(&corpus_json)
        .map_err(|e| anyhow::anyhow!("parsing corpus-notes.jsonl: {}", e))?;
    let memory_ref_queries: Vec<MinedMemoryRefRow> = fixtures::parse_jsonl(&queries_json)
        .map_err(|e| anyhow::anyhow!("parsing memory-ref-queries.jsonl: {}", e))?;
    let bad_cases: Vec<BadCaseRow> = fixtures::parse_jsonl(&bad_cases_json)
        .map_err(|e| anyhow::anyhow!("parsing bad-cases.jsonl: {}", e))?;

    let manifest = if manifest_path.exists() {
        let manifest_json = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        Some(
            serde_json::from_str::<FixtureManifest>(&manifest_json)
                .with_context(|| "parsing manifest.json")?,
        )
    } else {
        None
    };

    Ok(Phase1Fixtures {
        corpus_notes,
        memory_ref_queries,
        bad_cases,
        manifest,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;

    fn make_test_corpus_notes() -> Vec<CorpusNoteRow> {
        let json1 = r#"{"permalink":"cases/slot-lifecycle-race","title":"Slot lifecycle race condition","content":"When a slot is torn down while the supervisor is still processing setup callbacks, the lifecycle runner may observe a `SlotStatus::Released` guard violation.","note_type":"case","folder":"cases","status":"active","tags":["race-condition","slot","lifecycle"],"retrieval_anchor":"slot teardown race during supervisor setup","timestamps":{"created_at":"2026-06-01T10:00:00.000Z","updated_at":"2026-06-15T14:30:00.000Z","last_accessed":"2026-07-01T09:00:00.000Z"},"confidence":0.85,"embedding":{"content_hash":"abc123def456","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.1,0.2,0.3]},"labels":[{"entity_type":"concept","name":"race condition"},{"entity_type":"file","name":"slot/lifecycle.rs"}],"graph_edges":[{"source_permalink":"cases/slot-lifecycle-race","target_permalink":"patterns/supervisor-guard","kind":"builds_on","weight":1.0}],"expected_signals":{"vector":true,"lexical":true,"temporal":true,"graph":true,"entity":true,"task_affinity":false}}"#;
        let json2 = r#"{"permalink":"patterns/supervisor-guard","title":"Supervisor guard pattern","content":"Guard pattern content for supervisor lifecycle management","note_type":"pattern","folder":"patterns","status":"active","tags":["guard"],"timestamps":{"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z","last_accessed":"2026-01-01T00:00:00.000Z"},"confidence":0.9,"embedding":{"content_hash":"hash456","model_version":"text-embedding-3-small-v1","embedding_dim":3,"vector":[0.4,0.5,0.6]},"labels":[{"entity_type":"concept","name":"guard"}],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":true,"task_affinity":false}}"#;
        vec![
            serde_json::from_str(json1).unwrap(),
            serde_json::from_str(json2).unwrap(),
        ]
    }

    fn make_test_queries() -> Vec<MinedMemoryRefRow> {
        let json = r#"{"query_id":"task-abc123","query_text":"How do I handle slot teardown race conditions?","task_id":"abc123","memory_refs":["cases/slot-lifecycle-race","patterns/supervisor-guard"],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":true,"entity":true,"task_affinity":true}}"#;
        vec![serde_json::from_str(json).unwrap()]
    }

    fn make_test_bad_cases() -> Vec<BadCaseRow> {
        let json1 = r#"{"case_id":"bc-001","query_text":"What happens when a note is over-decayed?","case_type":"over_decay_threshold","expected_behavior":"Note should remain in recall@10 despite being older than decay window","task_id":null,"relevant_note_permalinks":["cases/slot-lifecycle-race"],"expected_signals":{"vector":true,"lexical":false,"temporal":true,"graph":false,"entity":false,"task_affinity":false},"tags":["decay","high-priority"]}"#;
        let json2 = r#"{"case_id":"bc-002","query_text":"Which pattern builds on the supervisor guard?","case_type":"graph_entity_influenced","expected_behavior":"Graph proximity or entity overlap should surface the note in recall@5","task_id":null,"relevant_note_permalinks":["patterns/supervisor-guard"],"expected_signals":{"vector":false,"lexical":true,"temporal":false,"graph":true,"entity":true,"task_affinity":false},"tags":["graph","entity"]}"#;
        let json3 = r#"{"case_id":"bc-003","query_text":"What memory refs are associated with task xyz?","case_type":"task_affinity_influenced","expected_behavior":"Task-affinity signal should surface the note in recall@5 when task_id is provided","task_id":"xyz","relevant_note_permalinks":["cases/slot-lifecycle-race"],"expected_signals":{"vector":true,"lexical":false,"temporal":false,"graph":false,"entity":false,"task_affinity":true},"tags":["task-affinity"]}"#;
        vec![
            serde_json::from_str(json1).unwrap(),
            serde_json::from_str(json2).unwrap(),
            serde_json::from_str(json3).unwrap(),
        ]
    }

    fn make_test_fixtures() -> Phase1Fixtures {
        Phase1Fixtures {
            corpus_notes: make_test_corpus_notes(),
            memory_ref_queries: make_test_queries(),
            bad_cases: make_test_bad_cases(),
            manifest: None,
        }
    }

    #[test]
    fn validate_fixtures_passes_for_valid_fixture_set() {
        let fixtures = make_test_fixtures();
        validate_fixtures(&fixtures).expect("validation should pass");
    }

    #[test]
    fn task_affinity_bad_case_only_task_seeds_memory_refs_from_relevant_notes() {
        let mut fixtures = make_test_fixtures();
        fixtures.memory_ref_queries.clear();
        fixtures.bad_cases.retain(|case| case.case_id == "bc-003");

        validate_fixtures(&fixtures).expect("bad-case-only task-affinity fixture should validate");

        let note_id_by_permalink = HashMap::from([
            (
                "cases/slot-lifecycle-race".to_string(),
                "note-id-slot-race".to_string(),
            ),
            (
                "patterns/supervisor-guard".to_string(),
                "note-id-supervisor-guard".to_string(),
            ),
        ]);

        let memory_refs = task_memory_ref_note_ids(&fixtures, "xyz", &note_id_by_permalink);

        assert_eq!(memory_refs, vec!["note-id-slot-race".to_string()]);
    }

    #[test]
    fn task_memory_refs_union_mined_queries_and_task_affinity_bad_cases_deterministically() {
        let fixtures = make_test_fixtures();
        let note_id_by_permalink = HashMap::from([
            (
                "cases/slot-lifecycle-race".to_string(),
                "note-id-slot-race".to_string(),
            ),
            (
                "patterns/supervisor-guard".to_string(),
                "note-id-supervisor-guard".to_string(),
            ),
        ]);

        let mined_refs = task_memory_ref_note_ids(&fixtures, "abc123", &note_id_by_permalink);
        let bad_case_refs = task_memory_ref_note_ids(&fixtures, "xyz", &note_id_by_permalink);

        assert_eq!(
            mined_refs,
            vec![
                "note-id-slot-race".to_string(),
                "note-id-supervisor-guard".to_string()
            ]
        );
        assert_eq!(bad_case_refs, vec!["note-id-slot-race".to_string()]);
    }

    #[test]
    fn validate_fixtures_fails_on_empty_created_at() {
        let mut fixtures = make_test_fixtures();
        fixtures.corpus_notes[0].timestamps.created_at = String::new();
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("created_at is empty")
        );
    }

    #[test]
    fn validate_fixtures_fails_on_invalid_status() {
        let mut fixtures = make_test_fixtures();
        fixtures.corpus_notes[0].status = "invalid".to_string();
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid status"));
    }

    #[test]
    fn validate_fixtures_fails_when_graph_signal_claimed_but_no_edges() {
        let mut fixtures = make_test_fixtures();
        // The second note (patterns/supervisor-guard) has graph=false, so no
        // edges. Force graph=true.
        fixtures.corpus_notes[1].expected_signals.graph = true;
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("graph signal claimed but no graph_edges")
        );
    }

    #[test]
    fn validate_fixtures_fails_when_entity_signal_claimed_but_no_labels() {
        let mut fixtures = make_test_fixtures();
        fixtures.corpus_notes[1].expected_signals.entity = false;
        // Add entity signal to first note's expected but remove labels
        // Actually, let's use the second note: set entity=true, clear labels
        fixtures.corpus_notes[1].expected_signals.entity = true;
        fixtures.corpus_notes[1].labels = vec![];
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("entity signal claimed but no labels")
        );
    }

    #[test]
    fn validate_fixtures_fails_when_vector_signal_claimed_but_no_embedding() {
        let mut fixtures = make_test_fixtures();
        fixtures.corpus_notes[0].expected_signals.vector = true;
        fixtures.corpus_notes[0].embedding = None;
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("vector signal claimed but no embedding")
        );
    }

    #[test]
    fn validate_fixtures_fails_on_embedding_dim_mismatch() {
        let mut fixtures = make_test_fixtures();
        fixtures.corpus_notes[0]
            .embedding
            .as_mut()
            .unwrap()
            .embedding_dim = 10;
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("embedding_dim 10 != vector length 3")
        );
    }

    #[test]
    fn validate_fixtures_fails_when_task_affinity_claimed_but_no_task_id() {
        let mut fixtures = make_test_fixtures();
        fixtures.memory_ref_queries[0]
            .expected_signals
            .task_affinity = true;
        fixtures.memory_ref_queries[0].task_id = None;
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("task_affinity signal claimed but no task_id")
        );
    }

    #[test]
    fn validate_fixtures_fails_on_missing_corpus_permalink() {
        let mut fixtures = make_test_fixtures();
        fixtures.memory_ref_queries[0]
            .memory_refs
            .push("nonexistent/note".to_string());
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found in corpus")
        );
    }

    #[test]
    fn validate_fixtures_fails_on_empty_query_text() {
        let mut fixtures = make_test_fixtures();
        fixtures.memory_ref_queries[0].query_text = String::new();
        let result = validate_fixtures(&fixtures);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("query_text is empty")
        );
    }
}
