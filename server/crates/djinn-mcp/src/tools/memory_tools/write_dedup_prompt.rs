use djinn_core::models::NoteDedupCandidate;

use super::write_dedup_types::{MemoryWriteDedupDecision, MemoryWriteDedupDecisionInput};

pub(super) const MEMORY_WRITE_DEDUP_SYSTEM: &str = "You are deciding whether a new knowledge-base note should create a new note, reuse an existing candidate, or merge into an existing candidate. Respond with JSON only.\n\
Schema: {\"action\":\"create_new|reuse_existing|merge_into_existing\",\"candidate_id\":\"optional candidate id\",\"merged_title\":\"required for merge_into_existing\",\"merged_content\":\"required for merge_into_existing\"}.\n\
Choose create_new when the draft is materially distinct.\n\
Choose reuse_existing when the draft is effectively the same note.\n\
Choose merge_into_existing when the draft should update an existing note with combined content.";

#[derive(Debug, serde::Deserialize)]
struct MemoryWriteDedupDecisionPayload {
    action: String,
    candidate_id: Option<String>,
    merged_title: Option<String>,
    merged_content: Option<String>,
}

pub(super) fn render_memory_write_dedup_prompt(
    input: &MemoryWriteDedupDecisionInput<'_>,
) -> String {
    let mut prompt = format!(
        "Incoming note:\n- project: {}\n- title: {}\n- type: {}\n\nContent:\n{}\n\nCandidates:\n",
        input.project_path, input.title, input.note_type, input.content
    );

    for candidate in input.candidates {
        prompt.push_str(&format_candidate(candidate));
        prompt.push('\n');
    }

    prompt.push_str(
        "Decide whether to create a new note, reuse an existing candidate, or merge into an existing candidate. Respond with JSON only.",
    );

    prompt
}

fn format_candidate(candidate: &NoteDedupCandidate) -> String {
    let summary = candidate
        .overview
        .as_deref()
        .or(candidate.abstract_.as_deref())
        .unwrap_or("");

    format!(
        "- id: {}\n  title: {}\n  permalink: {}\n  score: {:.3}\n  summary: {}",
        candidate.id, candidate.title, candidate.permalink, candidate.score, summary
    )
}

pub(crate) fn parse_memory_write_dedup_decision(
    raw: &str,
) -> Result<MemoryWriteDedupDecision, String> {
    let payload = serde_json::from_str::<MemoryWriteDedupDecisionPayload>(raw.trim())
        .map_err(|error| format!("failed to parse dedup decision JSON: {error}"))?;

    match payload.action.trim().to_ascii_lowercase().as_str() {
        "create_new" => Ok(MemoryWriteDedupDecision::CreateNew),
        "reuse_existing" => Ok(MemoryWriteDedupDecision::ReuseExisting {
            candidate_id: payload
                .candidate_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "reuse_existing requires candidate_id".to_string())?,
        }),
        "merge_into_existing" => Ok(MemoryWriteDedupDecision::MergeIntoExisting {
            candidate_id: payload
                .candidate_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "merge_into_existing requires candidate_id".to_string())?,
            merged_title: payload
                .merged_title
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "merge_into_existing requires merged_title".to_string())?,
            merged_content: payload
                .merged_content
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "merge_into_existing requires merged_content".to_string())?,
        }),
        other => Err(format!("unknown dedup action: {other}")),
    }
}
