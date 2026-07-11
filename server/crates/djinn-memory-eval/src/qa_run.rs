//! Phase 2 QA execution: real retrieval + 2000-character injection capture.
//!
//! This module runs QA pairs extracted from Phase 1 corpus notes through the
//! real `NoteRepository::search` pipeline and the same
//! [`djinn_slot::helpers::format_knowledge_notes`] rendering used at session
//! startup. It records per-QA retrieval metrics (hit, gold rank, permalinks,
//! context recall, note type, age bucket) without calling any LLM provider.
//!
//! # Determinism
//!
//! No LLM calls are made. All search and formatting uses the production
//! pipeline paths loaded from committed fixtures. The injected payload hash
//! is SHA-256 of the rendered text for artifact-safe comparison.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tracing::info;

use djinn_db::database::Database;
use djinn_db::repositories::note::{NoteRepository, NoteSearchParams};
use djinn_memory::Note;

use crate::fixtures::Phase1Fixtures;
use crate::loader::{self, LoadedFixtureState};
use crate::metrics::AgeBucket;
use crate::qa::{QaExtractionReport, QaPair, extract_qa_pairs};

// ── Constants ──────────────────────────────────────────────────────────────

/// Top-k for QA search — same as session-start retrieval.
const QA_TOP_K: usize = 10;

/// Character budget for `format_knowledge_notes` — matches session-start
/// injection at `djinn-agent/src/actors/slot/lifecycle/prompt_context.rs:397`.
const INJECTION_BUDGET_CHARS: usize = 2000;

// ── Per-QA output record ───────────────────────────────────────────────────

/// Per-QA-pair output from the Phase 2 retrieval and injection pipeline.
///
/// Each record captures both the retrieval-layer metrics (did the gold note
/// appear in top-k, at what rank) and the injection-layer metrics (was the
/// gold answer text present in the rendered 2000-char payload).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QaRunRecord {
    /// Stable QA identifier from `qa::QaPair::qa_id`.
    pub qa_id: String,
    /// Permalink of the source gold note.
    pub source_permalink: String,
    /// Note type of the source note (`"pitfall"` or `"case"`).
    pub note_type: String,
    /// The QA question text assembled from symptom/situation sections.
    pub question: String,
    /// Whether the gold note appeared anywhere in top-k search results.
    pub retrieval_hit: bool,
    /// 1-based rank of the gold note in search results, or `None` if absent.
    pub gold_rank: Option<usize>,
    /// Permalinks of all top-k search results, in rank order.
    pub result_permalinks: Vec<String>,
    /// SHA-256 hex digest of the rendered injected payload.
    pub injected_payload_hash: String,
    /// Full 2000-character session-start memory payload rendered by
    /// `format_knowledge_notes`. Phase 2 LLM judge passes consume this exact
    /// payload; Phase 1 reports/baselines remain separate and deterministic.
    pub injected_payload: String,
    /// Preview of the first 200 characters of the rendered injected payload.
    pub injected_payload_preview: String,
    /// Whether the gold answer text appears in the rendered 2000-char payload.
    /// This is the primary Phase 2 context-recall metric.
    pub context_recall: bool,
    /// Age bucket of the source note (computed from created_at → updated_at).
    pub age_bucket: AgeBucket,
    /// Age of the source note in days.
    pub age_days: i64,
    /// Confidence of the source note.
    pub confidence: f64,
}

// ── Full QA run output ─────────────────────────────────────────────────────

/// Full Phase 2 QA run output.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QaRunOutput {
    /// Per-QA-pair records.
    pub records: Vec<QaRunRecord>,
    /// QA extraction report (pairs extracted, notes skipped).
    pub extraction: QaExtractionReport,
    /// Number of corpus notes loaded into the database.
    pub corpus_note_count: usize,
    /// Total QA pairs processed.
    pub qa_count: usize,
    /// Number of QA pairs where the gold note was found in top-k.
    pub retrieval_hit_count: usize,
    /// Number of QA pairs where the gold answer was present in the rendered
    /// 2000-char payload (context recall = true).
    pub context_recall_count: usize,
}

// ── Age bucket helper ──────────────────────────────────────────────────────

/// Classify a note age in whole days into the standard age bucket.
fn age_bucket_from_days(age_days: i64) -> AgeBucket {
    AgeBucket::from_days(age_days.max(0) as u32)
}

// ── QA run entry point ─────────────────────────────────────────────────────

/// Execute the Phase 2 QA run against fixtures on disk.
///
/// 1. Loads Phase 1 fixtures from the crate's `fixtures/` directory.
/// 2. Extracts QA pairs from eligible pitfall/case corpus notes.
/// 3. Creates an isolated in-memory database and loads all fixtures.
/// 4. For each QA pair, executes real `NoteRepository::search` with top-k 10.
/// 5. Fetches full `Note` objects for each search result.
/// 6. Renders through `format_knowledge_notes(&notes, 2000)` (byte-compatible
///    with session-start injection).
/// 7. Records retrieval hit, gold rank, permalinks, payload hash/preview,
///    context recall, note type, and age bucket per QA pair.
pub async fn execute_qa_run(crate_root: &Path) -> Result<QaRunOutput> {
    let fixtures =
        loader::load_fixtures_from_disk(crate_root).context("loading fixtures from disk")?;

    execute_qa_run_with_fixtures(&fixtures).await
}

/// Execute the QA run with pre-loaded fixtures (useful for testing).
pub async fn execute_qa_run_with_fixtures(fixtures: &Phase1Fixtures) -> Result<QaRunOutput> {
    // 1. Validate fixtures
    info!("validating fixtures for QA run...");
    loader::validate_fixtures(fixtures).context("fixture validation failed")?;
    info!("fixture validation passed");

    // 2. Extract QA pairs from corpus
    let extraction = extract_qa_pairs(&fixtures.corpus_notes);
    info!(
        pairs = extraction.pairs.len(),
        skipped = extraction.skipped.len(),
        eligible = extraction.eligible_count,
        "QA pairs extracted"
    );

    if extraction.pairs.is_empty() {
        info!("no QA pairs extracted; returning empty output");
        return Ok(QaRunOutput {
            records: Vec::new(),
            extraction,
            corpus_note_count: fixtures.corpus_notes.len(),
            qa_count: 0,
            retrieval_hit_count: 0,
            context_recall_count: 0,
        });
    }

    // 3. Create isolated database and load fixtures
    let db = Database::open_in_memory().context("opening isolated test database")?;
    let state = loader::load_fixtures(&db, fixtures)
        .await
        .context("loading fixtures into database")?;

    info!(
        project_id = %state.project.id,
        notes = state.note_id_by_permalink.len(),
        "fixtures loaded for QA run"
    );

    let repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    // 4–7. Process each QA pair
    let mut records = Vec::with_capacity(extraction.pairs.len());

    for qa_pair in &extraction.pairs {
        let record = execute_qa_pair(&repo, &state, qa_pair)
            .await
            .with_context(|| format!("executing QA pair '{}'", qa_pair.qa_id))?;
        records.push(record);
    }

    let retrieval_hit_count = records.iter().filter(|r| r.retrieval_hit).count();
    let context_recall_count = records.iter().filter(|r| r.context_recall).count();
    let qa_count = records.len();

    info!(
        qa_count,
        retrieval_hits = retrieval_hit_count,
        context_recalls = context_recall_count,
        "QA run completed"
    );

    Ok(QaRunOutput {
        records,
        extraction,
        corpus_note_count: fixtures.corpus_notes.len(),
        qa_count,
        retrieval_hit_count,
        context_recall_count,
    })
}

// ── Per-QA pair execution ──────────────────────────────────────────────────

/// Execute the retrieval and injection pipeline for a single QA pair.
async fn execute_qa_pair(
    repo: &NoteRepository,
    state: &LoadedFixtureState,
    qa_pair: &QaPair,
) -> Result<QaRunRecord> {
    // Step 1: Execute real search with the QA question
    let search_results = repo
        .search(NoteSearchParams {
            project_id: &state.project.id,
            query: &qa_pair.question,
            task_id: None,
            folder: None,
            note_type: None,
            limit: QA_TOP_K,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .context("executing NoteRepository::search for QA question")?;

    let result_permalinks: Vec<String> =
        search_results.iter().map(|r| r.permalink.clone()).collect();

    // Step 2: Check retrieval hit — is the gold note in top-k?
    let gold_rank = result_permalinks
        .iter()
        .position(|p| p == &qa_pair.source_permalink)
        .map(|pos| pos + 1); // 1-based
    let retrieval_hit = gold_rank.is_some();

    // Step 3: Fetch full Note objects for each search result
    let mut notes: Vec<Note> = Vec::with_capacity(search_results.len());
    for result in &search_results {
        if let Some(note) = repo
            .get_by_permalink(&state.project.id, &result.permalink)
            .await
            .with_context(|| format!("fetching full Note for permalink '{}'", result.permalink))?
        {
            notes.push(note);
        }
    }

    // Step 4: Render through format_knowledge_notes (byte-compatible)
    let rendered = djinn_slot::helpers::format_knowledge_notes(&notes, INJECTION_BUDGET_CHARS);

    // Step 5: Check context recall — gold answer present in rendered payload?
    // Use a normalized lowercase comparison for robustness.
    let rendered_lower = rendered.to_lowercase();
    let gold_answer_lower = qa_pair.gold_answer.to_lowercase();
    let context_recall =
        !gold_answer_lower.is_empty() && rendered_lower.contains(&gold_answer_lower);

    // Step 6: Compute payload hash and preview
    let payload_hash = {
        let mut hasher = Sha256::new();
        hasher.update(rendered.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let preview_len = rendered.len().min(200);
    let payload_preview = rendered[..preview_len].to_string();

    // Step 7: Compute age bucket
    let age_bucket = age_bucket_from_days(qa_pair.age_days);

    Ok(QaRunRecord {
        qa_id: qa_pair.qa_id.clone(),
        source_permalink: qa_pair.source_permalink.clone(),
        note_type: qa_pair.note_type.clone(),
        question: qa_pair.question.clone(),
        retrieval_hit,
        gold_rank,
        result_permalinks,
        injected_payload_hash: payload_hash,
        injected_payload: rendered,
        injected_payload_preview: payload_preview,
        context_recall,
        age_bucket,
        age_days: qa_pair.age_days,
        confidence: qa_pair.confidence,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fixtures::{CorpusNoteRow, LifecycleTimestamps};

    /// Build a well-formed pitfall note with all required sections.
    fn pitfall_note() -> CorpusNoteRow {
        let content = r#"## Trigger / smell

The agent emits a slot lifecycle timeout error when a model slot is released while callbacks are still pending.

## Failure mode

The supervisor panics with a guard violation because the slot status is `Released` but the callback expects `Active`.

## Observable symptoms

- Log lines contain `SlotStatus::Released guard violation`
- The agent crashes with exit code 137
- Slot metrics show premature release counts

## Prevention

Always check `slot.status()` before dispatching async callbacks. Use the `with_slot_guard` helper to hold the slot alive for the callback's lifetime.

## Recovery

Restart the slot supervisor with `--reset-sessions`. The guard violation is non-fatal if caught by the outer supervision tree.

## Related

- patterns/supervisor-guard
- cases/slot-lifecycle-race"#;

        CorpusNoteRow {
            permalink: "pitfalls/slot-guard-violation".to_string(),
            title: "Slot guard violation pitfall".to_string(),
            content: content.to_string(),
            note_type: "pitfall".to_string(),
            folder: "pitfalls".to_string(),
            status: "active".to_string(),
            tags: vec!["slot".to_string(), "guard".to_string()],
            retrieval_anchor: Some("slot guard violation during callback dispatch".to_string()),
            timestamps: LifecycleTimestamps {
                created_at: "2026-06-01T10:00:00.000Z".to_string(),
                updated_at: "2026-07-01T10:00:00.000Z".to_string(),
                last_accessed: "2026-07-10T10:00:00.000Z".to_string(),
            },
            confidence: 0.9,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    /// Build a well-formed case note with all required sections.
    fn case_note() -> CorpusNoteRow {
        let content = r#"## Situation

A memory retrieval query returns stale notes because the Bayesian confidence decay rate is too aggressive for recently-updated notes.

## Constraint

The decay function must not change the scoring contract for notes older than 90 days; only the 0–30 day window can be adjusted.

## Approach taken

Introduced a tiered decay curve: linear for the first 30 days, exponential after. Updated the `decay_signal_for_elapsed_days` function to accept a tier boundary parameter.

## Result

Post-change, notes updated within 7 days now rank in the top 5 for relevant queries where they previously dropped to rank 15+. No regression for the >90 day cohort.

## Why it worked / failed

The tiered approach preserved the tail behavior that downstream consumers depend on while protecting recently-updated notes from premature demotion.

## Reusable lesson

When adjusting decay functions, always tier the curve so the long tail is preserved. Test against the >90 day cohort as a regression guard.

## Related

- decisions/decay-rate-adrs
- pitfalls/over-decay"#;

        CorpusNoteRow {
            permalink: "cases/decay-rate-adjustment".to_string(),
            title: "Decay rate tiered adjustment case".to_string(),
            content: content.to_string(),
            note_type: "case".to_string(),
            folder: "cases".to_string(),
            status: "active".to_string(),
            tags: vec!["decay".to_string(), "scoring".to_string()],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-05-15T00:00:00.000Z".to_string(),
                updated_at: "2026-07-10T00:00:00.000Z".to_string(),
                last_accessed: "2026-07-10T00:00:00.000Z".to_string(),
            },
            confidence: 0.85,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    /// A filler note with unrelated content (to add noise to the corpus).
    fn filler_note() -> CorpusNoteRow {
        CorpusNoteRow {
            permalink: "patterns/unrelated-filler".to_string(),
            title: "Unrelated filler pattern".to_string(),
            content: "This is a note about something completely unrelated to slots or decay."
                .to_string(),
            note_type: "pattern".to_string(),
            folder: "patterns".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-01-01T00:00:00.000Z".to_string(),
                updated_at: "2026-01-01T00:00:00.000Z".to_string(),
                last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
            },
            confidence: 0.5,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    /// Build a minimal Phase1Fixtures with our test notes.
    fn test_fixtures() -> Phase1Fixtures {
        Phase1Fixtures {
            corpus_notes: vec![pitfall_note(), case_note(), filler_note()],
            memory_ref_queries: vec![],
            bad_cases: vec![],
            manifest: None,
        }
    }

    #[test]
    fn age_bucket_from_days_classifies_correctly() {
        assert_eq!(age_bucket_from_days(0), AgeBucket::Under7d);
        assert_eq!(age_bucket_from_days(6), AgeBucket::Under7d);
        assert_eq!(age_bucket_from_days(7), AgeBucket::Days7to30);
        assert_eq!(age_bucket_from_days(29), AgeBucket::Days7to30);
        assert_eq!(age_bucket_from_days(30), AgeBucket::Days30to90);
        assert_eq!(age_bucket_from_days(89), AgeBucket::Days30to90);
        assert_eq!(age_bucket_from_days(90), AgeBucket::OverDecayThreshold);
        assert_eq!(age_bucket_from_days(365), AgeBucket::OverDecayThreshold);
        // Negative age clamps to 0
        assert_eq!(age_bucket_from_days(-5), AgeBucket::Under7d);
    }

    #[test]
    fn qa_extraction_from_test_fixtures_produces_pairs() {
        let fixtures = test_fixtures();
        let report = extract_qa_pairs(&fixtures.corpus_notes);
        assert_eq!(
            report.pairs.len(),
            2,
            "should extract pitfall and case QA pairs"
        );
        assert_eq!(report.eligible_count, 2);
    }

    #[tokio::test]
    async fn qa_run_produces_records_for_extracted_pairs() {
        let fixtures = test_fixtures();
        let output = execute_qa_run_with_fixtures(&fixtures)
            .await
            .expect("QA run should succeed");

        assert_eq!(output.qa_count, 2, "should process 2 QA pairs");
        assert_eq!(output.records.len(), 2);
        assert_eq!(output.corpus_note_count, 3);
        assert_eq!(output.extraction.pairs.len(), 2);

        // Verify each record has the expected fields populated
        for record in &output.records {
            assert!(
                !record.result_permalinks.is_empty(),
                "should have search results"
            );
            assert!(
                !record.injected_payload_hash.is_empty(),
                "should have payload hash"
            );
            assert!(
                record.injected_payload_hash.len() == 64,
                "SHA-256 hex should be 64 chars"
            );
        }
    }

    #[tokio::test]
    async fn context_recall_passes_when_gold_answer_in_rendered_payload() {
        // Build a pitfall note where the gold answer text (Prevention body +
        // "\n\n" + Recovery body) appears verbatim in the rendered 200-char
        // summary. The trick: the question sections are placed first in the
        // content. By replicating the answer-body text in the Trigger section
        // body, the gold answer appears as a substring of the rendered payload.
        //
        // Prevention body = "Prevention text."
        // Recovery body   = "Recovery text."
        // Gold answer     = "Prevention text.\n\nRecovery text."
        //
        // Trigger body    = "Prevention text.\n\nRecovery text."  ← matches
        let content = "\
## Trigger / smell\n\n\
Prevention text.\n\n\
Recovery text.\n\n\
## Failure mode\n\n\
Fails.\n\n\
## Observable symptoms\n\n\
Crash.\n\n\
## Prevention\n\n\
Prevention text.\n\n\
## Recovery\n\n\
Recovery text.\n\n\
## Related\n\n\
- x";

        let note = CorpusNoteRow {
            permalink: "pitfalls/recall-pass".to_string(),
            title: "Recall pass pitfall".to_string(),
            content: content.to_string(),
            note_type: "pitfall".to_string(),
            folder: "pitfalls".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-06-01T10:00:00.000Z".to_string(),
                updated_at: "2026-07-01T10:00:00.000Z".to_string(),
                last_accessed: "2026-07-10T10:00:00.000Z".to_string(),
            },
            confidence: 0.9,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        };

        let fixtures = Phase1Fixtures {
            corpus_notes: vec![note],
            memory_ref_queries: vec![],
            bad_cases: vec![],
            manifest: None,
        };

        let output = execute_qa_run_with_fixtures(&fixtures)
            .await
            .expect("QA run should succeed");

        assert_eq!(output.qa_count, 1);

        let record = &output.records[0];

        // The gold answer "Prevention text.\n\nRecovery text." appears in
        // the Trigger section body, which falls within the first 200 chars
        // of content. format_knowledge_notes renders content[..200] as the
        // summary for high-confidence notes, so the gold answer IS present
        // in the rendered 2000-char injection payload.
        assert!(
            record.context_recall,
            "context_recall should be true when gold answer is present in the \
             rendered payload. record: {:?}",
            record
        );

        assert!(
            !record.injected_payload_preview.is_empty(),
            "should have non-empty payload preview"
        );
    }

    #[tokio::test]
    async fn context_recall_can_fail_when_gold_answer_truncated() {
        // Build a note with a very long gold answer that will NOT fit in the
        // truncated summary (only first ~100 chars of content are used for
        // low-confidence notes with no abstract).
        let long_content = format!(
            "## Trigger / smell\n\nThe trigger text.\n\n\
             ## Failure mode\n\nThe failure mode text.\n\n\
             ## Observable symptoms\n\nSymptom text.\n\n\
             ## Prevention\n\n{}\n\n\
             ## Recovery\n\n{}\n\n\
             ## Related\n\n- some/related",
            "A".repeat(300),
            "B".repeat(300),
        );

        let long_pitfall = CorpusNoteRow {
            permalink: "pitfalls/long-answer-note".to_string(),
            title: "Long answer pitfall".to_string(),
            content: long_content,
            note_type: "pitfall".to_string(),
            folder: "pitfalls".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-06-01T10:00:00.000Z".to_string(),
                updated_at: "2026-07-01T10:00:00.000Z".to_string(),
                last_accessed: "2026-07-10T10:00:00.000Z".to_string(),
            },
            confidence: 0.5, // Low confidence → uses first 100 chars of content
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        };

        let fixtures = Phase1Fixtures {
            corpus_notes: vec![long_pitfall],
            memory_ref_queries: vec![],
            bad_cases: vec![],
            manifest: None,
        };

        let output = execute_qa_run_with_fixtures(&fixtures)
            .await
            .expect("QA run should succeed");

        assert_eq!(output.qa_count, 1);

        let record = &output.records[0];

        // The gold answer is ~600 chars ("A"*300 + "\n\n" + "B"*300),
        // but the rendered summary for low-confidence notes uses only the
        // first 100 chars of content (the trigger/symptom section, not
        // the prevention/recovery sections). So context_recall should be
        // false.
        assert!(
            !record.context_recall,
            "context recall should be false when gold answer is long and note is low-confidence \
             (truncated to first 100 chars of content, which misses Prevention/Recovery). \
             Rendered preview: {}",
            record.injected_payload_preview
        );
    }

    #[tokio::test]
    async fn context_recall_passes_when_gold_answer_in_short_summary() {
        // Build a pitfall note with very short content where the gold answer
        // text (Prevention body + "\n\n" + Recovery body) appears verbatim in
        // the rendered 200-char summary window.
        //
        // The key: the question sections come first in the content, and the
        // Failure mode body replicates the gold answer text so that it appears
        // as a substring of the rendered payload.
        //
        // Prevention body = "Lock the slot before dispatch."
        // Recovery body   = "Restart with --reset-sessions."
        // Gold answer     = "Lock the slot before dispatch.\n\nRestart with --reset-sessions."
        //
        // Failure mode body contains the same text → matches in rendered summary
        let content = "\
## Trigger / smell\n\n\
Slot crashes.\n\n\
## Failure mode\n\n\
Lock the slot before dispatch.\n\n\
Restart with --reset-sessions.\n\n\
## Observable symptoms\n\n\
Exit 137.\n\n\
## Prevention\n\n\
Lock the slot before dispatch.\n\n\
## Recovery\n\n\
Restart with --reset-sessions.\n\n\
## Related\n\n\
- x";

        let note = CorpusNoteRow {
            permalink: "pitfalls/short-answer-note".to_string(),
            title: "Short answer pitfall".to_string(),
            content: content.to_string(),
            note_type: "pitfall".to_string(),
            folder: "pitfalls".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-06-01T10:00:00.000Z".to_string(),
                updated_at: "2026-07-01T10:00:00.000Z".to_string(),
                last_accessed: "2026-07-10T10:00:00.000Z".to_string(),
            },
            confidence: 0.9, // High confidence → uses first 200 chars of content
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        };

        let fixtures = Phase1Fixtures {
            corpus_notes: vec![note],
            memory_ref_queries: vec![],
            bad_cases: vec![],
            manifest: None,
        };

        let output = execute_qa_run_with_fixtures(&fixtures)
            .await
            .expect("QA run should succeed");

        assert_eq!(output.qa_count, 1);

        let record = &output.records[0];

        // The gold answer "Lock the slot before dispatch.\n\nRestart with
        // --reset-sessions." appears in the Failure mode section body, which
        // is within the first 200 chars of the short content. The rendered
        // summary (content[..200] for high-confidence notes) therefore
        // contains the gold answer text, making context_recall true.
        assert!(
            record.context_recall,
            "context_recall should be true when the exact gold answer text is \
             present in the rendered 2000-char payload. record: {:?}",
            record
        );
    }

    #[tokio::test]
    async fn qa_run_with_empty_corpus_produces_empty_output() {
        let fixtures = Phase1Fixtures {
            corpus_notes: vec![],
            memory_ref_queries: vec![],
            bad_cases: vec![],
            manifest: None,
        };

        let output = execute_qa_run_with_fixtures(&fixtures)
            .await
            .expect("empty QA run should succeed");

        assert_eq!(output.qa_count, 0);
        assert!(output.records.is_empty());
        assert!(output.extraction.pairs.is_empty());
    }

    #[test]
    fn qa_run_record_serde_round_trip() {
        let record = QaRunRecord {
            qa_id: "qa-pitfall-abc123def456".to_string(),
            source_permalink: "pitfalls/test".to_string(),
            note_type: "pitfall".to_string(),
            question: "test question".to_string(),
            retrieval_hit: true,
            gold_rank: Some(3),
            result_permalinks: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            injected_payload_hash: "deadbeef".to_string(),
            injected_payload: "full payload text".to_string(),
            injected_payload_preview: "preview text".to_string(),
            context_recall: false,
            age_bucket: AgeBucket::Days7to30,
            age_days: 15,
            confidence: 0.85,
        };

        let json = serde_json::to_string(&record).unwrap();
        let round_tripped: QaRunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record.qa_id, round_tripped.qa_id);
        assert_eq!(record.retrieval_hit, round_tripped.retrieval_hit);
        assert_eq!(record.gold_rank, round_tripped.gold_rank);
        assert_eq!(record.context_recall, round_tripped.context_recall);
        assert_eq!(record.age_bucket, round_tripped.age_bucket);
    }

    #[test]
    fn qa_run_output_serde_round_trip() {
        let output = QaRunOutput {
            records: vec![],
            extraction: QaExtractionReport::default(),
            corpus_note_count: 5,
            qa_count: 0,
            retrieval_hit_count: 0,
            context_recall_count: 0,
        };

        let json = serde_json::to_string(&output).unwrap();
        let round_tripped: QaRunOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.corpus_note_count, 5);
        assert_eq!(round_tripped.qa_count, 0);
    }
}
