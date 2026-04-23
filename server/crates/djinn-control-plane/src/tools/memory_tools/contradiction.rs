// Stage 2 contradiction analysis: LLM classifies candidate pairs and applies
// confidence signals and bilateral associations.

use djinn_core::events::EventBus;
use djinn_memory::ContradictionCandidate;
use djinn_db::{CONTRADICTION, NoteRepository, STALE_CITATION};
use djinn_provider::{CompletionRequest, complete, provider::LlmProvider, resolve_memory_provider};
use tokio::sync::mpsc;
use tracing::{info, warn};

const CLASSIFICATION_SYSTEM: &str =
    "Compare two knowledge base notes. Respond with JSON only: {\"relation\":\"compatible|contradicts|supersedes|elaborates\"}.
contradicts: the notes make incompatible claims about the same topic.
supersedes: Note A is a newer version that replaces Note B.
elaborates: Note A adds detail to or extends Note B.
compatible: the notes are complementary or unrelated.";

const CLASSIFICATION_MAX_TOKENS: u32 = 64;

/// Weight applied to the bilateral association for a contradicting pair.
const CONTRADICTS_WEIGHT: f64 = 0.5;
/// Weight applied to the superseded_by directional association.
const SUPERSEDES_WEIGHT: f64 = 0.8;
/// Weight applied to the elaborates association.
const ELABORATES_WEIGHT: f64 = 0.6;

#[derive(Debug, PartialEq, Eq)]
enum Classification {
    Compatible,
    Contradicts,
    Supersedes,
    Elaborates,
}

#[derive(serde::Deserialize)]
struct ClassificationPayload {
    relation: String,
}

fn parse_classification(text: &str) -> Classification {
    // Try JSON parse first, then fall back to keyword search
    let relation = serde_json::from_str::<ClassificationPayload>(text.trim())
        .map(|p| p.relation)
        .unwrap_or_else(|_| text.to_ascii_lowercase());

    match relation.trim().to_ascii_lowercase().as_str() {
        "contradicts" => Classification::Contradicts,
        "supersedes" => Classification::Supersedes,
        "elaborates" => Classification::Elaborates,
        _ => Classification::Compatible,
    }
}

fn render_classification_prompt(
    note_title: &str,
    note_summary: &str,
    cand_title: &str,
    cand_summary: &str,
) -> String {
    format!(
        "Note A title: {note_title}\nNote A summary: {note_summary}\n\nNote B title: {cand_title}\nNote B summary: {cand_summary}\n\nClassify the relationship between Note A and Note B."
    )
}

/// Input for the contradiction analysis worker task.
pub(crate) struct ContradictionAnalysisInput {
    pub note_id: String,
    pub note_title: String,
    pub note_summary: String,
    pub candidates: Vec<ContradictionCandidate>,
}

const CONTRADICTION_WORKER_QUEUE: usize = 32;

/// Spawn a background worker that processes contradiction analysis inputs from a channel.
///
/// The worker is triggered only when a `contradiction_candidates` event is emitted (stage 1),
/// guaranteeing that LLM classification is never triggered on every write — only when
/// structural candidates are detected (AC1).
pub(crate) fn spawn_contradiction_analysis_worker(
    db: djinn_db::Database,
) -> mpsc::Sender<ContradictionAnalysisInput> {
    let (tx, mut rx) = mpsc::channel::<ContradictionAnalysisInput>(CONTRADICTION_WORKER_QUEUE);
    tokio::spawn(async move {
        while let Some(input) = rx.recv().await {
            run_contradiction_analysis(db.clone(), input).await;
        }
    });
    tx
}

/// Run LLM-backed contradiction analysis for a note and its candidates.
///
/// Gracefully degrades when the LLM is unavailable — logs a warning and returns.
/// Per-candidate failures are logged and skipped; the remaining candidates are
/// still processed.
pub(crate) async fn run_contradiction_analysis(
    db: djinn_db::Database,
    input: ContradictionAnalysisInput,
) {
    let provider = match resolve_memory_provider(&db).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "contradiction analysis: LLM unavailable, skipping");
            return;
        }
    };
    run_contradiction_analysis_with_provider(db, input, provider.as_ref()).await;
}

/// Inner analysis loop with an injectable LLM provider.
///
/// Separated from `run_contradiction_analysis` to allow tests to inject a mock provider
/// without needing real LLM credentials in the database.
async fn run_contradiction_analysis_with_provider(
    db: djinn_db::Database,
    input: ContradictionAnalysisInput,
    provider: &dyn LlmProvider,
) {
    let repo = NoteRepository::new(db.clone(), EventBus::noop());

    for candidate in &input.candidates {
        let cand_note = match repo.get(&candidate.id).await {
            Ok(Some(n)) => n,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, candidate_id = %candidate.id, "contradiction analysis: failed to load candidate");
                continue;
            }
        };

        let cand_summary = cand_note
            .abstract_
            .clone()
            .unwrap_or_else(|| cand_note.content.chars().take(500).collect::<String>());

        let prompt = render_classification_prompt(
            &input.note_title,
            &input.note_summary,
            &cand_note.title,
            &cand_summary,
        );

        let response = match complete(
            provider,
            CompletionRequest {
                system: CLASSIFICATION_SYSTEM.to_string(),
                prompt,
                max_tokens: CLASSIFICATION_MAX_TOKENS,
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "contradiction analysis: classification failed");
                continue;
            }
        };

        let classification = parse_classification(&response.text);

        match classification {
            Classification::Contradicts => {
                info!(
                    note_id = %input.note_id,
                    candidate_id = %candidate.id,
                    "contradiction analysis: contradicts — applying CONTRADICTION signal to both"
                );
                // AC2: both notes get a CONTRADICTION (0.1) confidence signal.
                let _ = repo.update_confidence(&input.note_id, CONTRADICTION).await;
                let _ = repo.update_confidence(&candidate.id, CONTRADICTION).await;
                // AC2: bilateral association (one canonical row, queryable from either direction).
                let _ = repo
                    .upsert_association_min_weight(
                        &input.note_id,
                        &candidate.id,
                        CONTRADICTS_WEIGHT,
                    )
                    .await;
            }
            Classification::Supersedes => {
                // The LLM determined "Note A (input) supersedes Note B (candidate)".
                // AC3: the candidate is the superseded note — apply STALE_CITATION only to it.
                info!(
                    note_id = %input.note_id,
                    candidate_id = %candidate.id,
                    "contradiction analysis: supersedes — applying STALE_CITATION to superseded candidate"
                );
                let _ = repo.update_confidence(&candidate.id, STALE_CITATION).await;
                // AC3: superseded_by association at SUPERSEDES_WEIGHT.
                let _ = repo
                    .upsert_association_min_weight(&input.note_id, &candidate.id, SUPERSEDES_WEIGHT)
                    .await;
            }
            Classification::Elaborates => {
                info!(
                    note_id = %input.note_id,
                    candidate_id = %candidate.id,
                    "contradiction analysis: elaborates — creating association"
                );
                let _ = repo
                    .upsert_association_min_weight(&input.note_id, &candidate.id, ELABORATES_WEIGHT)
                    .await;
            }
            Classification::Compatible => {
                info!(
                    note_id = %input.note_id,
                    candidate_id = %candidate.id,
                    "contradiction analysis: compatible — no action"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
        tempfile::tempdir_in(base).expect("create server crate tempdir")
    }
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_core::message::{ContentBlock, Conversation};
    use djinn_memory::TypeRisk;
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use djinn_provider::provider::{StreamEvent, ToolChoice};
    use futures::stream;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Mock LLM provider ────────────────────────────────────────────────────────

    struct MockLlmProvider {
        response_text: String,
        call_count: AtomicUsize,
    }

    impl MockLlmProvider {
        fn new(response_text: impl Into<String>) -> Self {
            Self {
                response_text: response_text.into(),
                call_count: AtomicUsize::new(0),
            }
        }
    }

    impl LlmProvider for MockLlmProvider {
        fn name(&self) -> &str {
            "mock-contradiction-provider"
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [serde_json::Value],
            _tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let text = self.response_text.clone();
            Box::pin(async move {
                let events: Vec<anyhow::Result<StreamEvent>> = vec![
                    Ok(StreamEvent::Delta(ContentBlock::text(text))),
                    Ok(StreamEvent::Done),
                ];
                Ok(Box::pin(stream::iter(events))
                    as Pin<
                        Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                    >)
            })
        }
    }

    // ── Test helpers ─────────────────────────────────────────────────────────────

    async fn make_project_and_two_notes(
        db: &Database,
        tmp: &tempfile::TempDir,
    ) -> (String, String) {
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let repo = NoteRepository::new(db.clone(), EventBus::noop());

        let shared_content = "authentication jwt bearer oauth2 security middleware session \
                              authorization role permission scope grant deny policy enforcement";

        let note1 = repo
            .create(
                &project.id,
                tmp.path(),
                "Auth Token A",
                shared_content,
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let note2 = repo
            .create(
                &project.id,
                tmp.path(),
                "Auth Token B",
                shared_content,
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        (note1.id, note2.id)
    }

    fn make_candidate(id: &str, title: &str) -> ContradictionCandidate {
        ContradictionCandidate {
            id: id.to_string(),
            permalink: format!("patterns/{}", id),
            title: title.to_string(),
            folder: "patterns".into(),
            note_type: "pattern".into(),
            score: 10.0,
            risk: TypeRisk::High,
        }
    }

    // ── Unit tests ────────────────────────────────────────────────────────────────

    #[test]
    fn parse_classification_recognizes_all_relations() {
        assert_eq!(
            parse_classification(r#"{"relation":"contradicts"}"#),
            Classification::Contradicts
        );
        assert_eq!(
            parse_classification(r#"{"relation":"supersedes"}"#),
            Classification::Supersedes
        );
        assert_eq!(
            parse_classification(r#"{"relation":"elaborates"}"#),
            Classification::Elaborates
        );
        assert_eq!(
            parse_classification(r#"{"relation":"compatible"}"#),
            Classification::Compatible
        );
    }

    #[test]
    fn parse_classification_falls_back_to_compatible_on_unknown() {
        assert_eq!(
            parse_classification(r#"{"relation":"unknown"}"#),
            Classification::Compatible
        );
        assert_eq!(
            parse_classification("not json at all"),
            Classification::Compatible
        );
    }

    #[test]
    fn parse_classification_is_case_insensitive() {
        assert_eq!(
            parse_classification(r#"{"relation":"CONTRADICTS"}"#),
            Classification::Contradicts
        );
    }

    #[test]
    fn render_classification_prompt_contains_titles_and_summaries() {
        let prompt = render_classification_prompt("Note A", "Summary A", "Note B", "Summary B");
        assert!(prompt.contains("Note A"));
        assert!(prompt.contains("Summary A"));
        assert!(prompt.contains("Note B"));
        assert!(prompt.contains("Summary B"));
    }

    // ── Integration tests (AC2, AC3, AC4) ────────────────────────────────────────

    /// AC2 + AC4: when the LLM classifies a pair as `contradicts`, BOTH notes get
    /// the CONTRADICTION (0.1) confidence signal and a bilateral association at weight 0.5.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn contradicting_notes_both_get_confidence_reduction_and_bilateral_association() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (id1, id2) = make_project_and_two_notes(&db, &tmp).await;

        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note1_before = repo.get(&id1).await.unwrap().unwrap();
        let note2_before = repo.get(&id2).await.unwrap().unwrap();

        let provider = MockLlmProvider::new(r#"{"relation":"contradicts"}"#);
        let input = ContradictionAnalysisInput {
            note_id: id1.clone(),
            note_title: note1_before.title.clone(),
            note_summary: note1_before.content.chars().take(500).collect(),
            candidates: vec![make_candidate(&id2, &note2_before.title)],
        };

        run_contradiction_analysis_with_provider(db.clone(), input, &provider).await;

        let note1_after = repo.get(&id1).await.unwrap().unwrap();
        let note2_after = repo.get(&id2).await.unwrap().unwrap();

        // AC4: both notes must have reduced confidence
        assert!(
            note1_after.confidence < note1_before.confidence,
            "note1 confidence should decrease: {} -> {}",
            note1_before.confidence,
            note1_after.confidence
        );
        assert!(
            note2_after.confidence < note2_before.confidence,
            "note2 confidence should decrease: {} -> {}",
            note2_before.confidence,
            note2_after.confidence
        );

        // AC2: bilateral association at CONTRADICTS_WEIGHT, reachable from both sides
        let assocs1 = repo.get_associations_for_note(&id1).await.unwrap();
        assert!(
            !assocs1.is_empty(),
            "bilateral association must exist for note1"
        );
        assert!(
            assocs1.iter().any(|a| a.weight >= CONTRADICTS_WEIGHT),
            "association weight must be >= {CONTRADICTS_WEIGHT}"
        );
        let assocs2 = repo.get_associations_for_note(&id2).await.unwrap();
        assert!(
            !assocs2.is_empty(),
            "bilateral association must be reachable from note2"
        );
    }

    /// AC3: when the LLM classifies as `supersedes`, only the candidate (superseded note)
    /// gets STALE_CITATION; the input note (the superseding one) is unchanged.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supersedes_applies_stale_citation_only_to_superseded_candidate() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (id1, id2) = make_project_and_two_notes(&db, &tmp).await;

        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note1_before = repo.get(&id1).await.unwrap().unwrap();
        let note2_before = repo.get(&id2).await.unwrap().unwrap();

        let provider = MockLlmProvider::new(r#"{"relation":"supersedes"}"#);
        let input = ContradictionAnalysisInput {
            note_id: id1.clone(),
            note_title: note1_before.title.clone(),
            note_summary: note1_before.content.chars().take(500).collect(),
            candidates: vec![make_candidate(&id2, &note2_before.title)],
        };

        run_contradiction_analysis_with_provider(db.clone(), input, &provider).await;

        let note1_after = repo.get(&id1).await.unwrap().unwrap();
        let note2_after = repo.get(&id2).await.unwrap().unwrap();

        // AC3: the superseding note (input) confidence must be unchanged
        assert!(
            (note1_after.confidence - note1_before.confidence).abs() < 1e-9,
            "superseding note confidence should be unchanged: {} != {}",
            note1_before.confidence,
            note1_after.confidence
        );
        // AC3: the superseded candidate confidence must be reduced
        assert!(
            note2_after.confidence < note2_before.confidence,
            "superseded candidate confidence should decrease: {} -> {}",
            note2_before.confidence,
            note2_after.confidence
        );

        // AC3: superseded_by association at SUPERSEDES_WEIGHT
        let assocs = repo.get_associations_for_note(&id1).await.unwrap();
        assert!(!assocs.is_empty(), "superseded_by association must exist");
        assert!(
            assocs.iter().any(|a| a.weight >= SUPERSEDES_WEIGHT),
            "association weight must be >= {SUPERSEDES_WEIGHT}"
        );
    }

    /// Elaborates: only creates an association, no confidence change on either note.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn elaborates_creates_association_without_confidence_change() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (id1, id2) = make_project_and_two_notes(&db, &tmp).await;

        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note1_before = repo.get(&id1).await.unwrap().unwrap();
        let note2_before = repo.get(&id2).await.unwrap().unwrap();

        let provider = MockLlmProvider::new(r#"{"relation":"elaborates"}"#);
        let input = ContradictionAnalysisInput {
            note_id: id1.clone(),
            note_title: note1_before.title.clone(),
            note_summary: note1_before.content.chars().take(500).collect(),
            candidates: vec![make_candidate(&id2, &note2_before.title)],
        };

        run_contradiction_analysis_with_provider(db.clone(), input, &provider).await;

        let note1_after = repo.get(&id1).await.unwrap().unwrap();
        let note2_after = repo.get(&id2).await.unwrap().unwrap();

        assert!(
            (note1_after.confidence - note1_before.confidence).abs() < 1e-9,
            "elaborates: note1 confidence should be unchanged"
        );
        assert!(
            (note2_after.confidence - note2_before.confidence).abs() < 1e-9,
            "elaborates: note2 confidence should be unchanged"
        );

        let assocs = repo.get_associations_for_note(&id1).await.unwrap();
        assert!(!assocs.is_empty(), "elaborates association must exist");
    }

    /// Compatible: no confidence change, no association.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compatible_produces_no_changes() {
        let tmp = workspace_tempdir();
        let db = Database::open_in_memory().unwrap();
        let (id1, id2) = make_project_and_two_notes(&db, &tmp).await;

        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        let note1_before = repo.get(&id1).await.unwrap().unwrap();
        let note2_before = repo.get(&id2).await.unwrap().unwrap();

        let provider = MockLlmProvider::new(r#"{"relation":"compatible"}"#);
        let input = ContradictionAnalysisInput {
            note_id: id1.clone(),
            note_title: note1_before.title.clone(),
            note_summary: note1_before.content.chars().take(500).collect(),
            candidates: vec![make_candidate(&id2, &note2_before.title)],
        };

        run_contradiction_analysis_with_provider(db.clone(), input, &provider).await;

        let note1_after = repo.get(&id1).await.unwrap().unwrap();
        let note2_after = repo.get(&id2).await.unwrap().unwrap();

        assert!((note1_after.confidence - note1_before.confidence).abs() < 1e-9);
        assert!((note2_after.confidence - note2_before.confidence).abs() < 1e-9);

        let assocs = repo.get_associations_for_note(&id1).await.unwrap();
        assert!(
            assocs.is_empty(),
            "compatible: no association should be created"
        );
    }
}
