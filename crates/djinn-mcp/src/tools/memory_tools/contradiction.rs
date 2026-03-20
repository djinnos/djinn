// Stage 2 contradiction analysis: LLM classifies candidate pairs and applies
// confidence signals and bilateral associations.

use djinn_core::events::EventBus;
use djinn_core::models::ContradictionCandidate;
use djinn_db::{CONTRADICTION, NoteRepository, STALE_CITATION};
use djinn_provider::{CompletionRequest, complete, resolve_memory_provider};
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
    pub note_created_at: String,
    pub note_summary: String,
    pub candidates: Vec<ContradictionCandidate>,
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
    let repo = NoteRepository::new(db.clone(), EventBus::noop());

    let provider = match resolve_memory_provider(&db).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "contradiction analysis: LLM unavailable, skipping");
            return;
        }
    };

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
            provider.as_ref(),
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
                let _ = repo.update_confidence(&input.note_id, CONTRADICTION).await;
                let _ = repo.update_confidence(&candidate.id, CONTRADICTION).await;
                let _ = repo
                    .upsert_association_min_weight(&input.note_id, &candidate.id, CONTRADICTS_WEIGHT)
                    .await;
            }
            Classification::Supersedes => {
                // Note A (the new note) supersedes Note B (the candidate).
                // Mark the older note (candidate) as stale.
                info!(
                    note_id = %input.note_id,
                    candidate_id = %candidate.id,
                    "contradiction analysis: supersedes — applying STALE_CITATION to older note"
                );
                let older_id = if input.note_created_at <= cand_note.created_at {
                    &candidate.id
                } else {
                    &input.note_id
                };
                let _ = repo.update_confidence(older_id, STALE_CITATION).await;
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
    use super::*;

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
}
