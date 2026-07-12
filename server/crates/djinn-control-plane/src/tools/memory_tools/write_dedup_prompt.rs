use djinn_memory::NoteDedupCandidate;

use super::write_dedup_types::{MemoryWriteDedupDecision, MemoryWriteDedupDecisionInput};

/// Maximum Unicode scalar values from one candidate body included in the
/// write-dedup prompt. The repository still carries the complete body.
pub(super) const MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP: usize = 4_000;

pub(super) const MEMORY_WRITE_DEDUP_SYSTEM: &str = "You are deciding whether a new knowledge-base note should create a new note, reuse an existing candidate, merge into an existing candidate, or supersede an existing candidate. Respond with JSON only.\n\
Schema: {\"action\":\"create_new|reuse_existing|merge_into_existing|supersede_existing\",\"candidate_id\":\"required for reuse_existing, merge_into_existing, and supersede_existing\",\"merged_title\":\"required for merge_into_existing\",\"merged_content\":\"required for merge_into_existing\",\"reason\":\"required for supersede_existing\"}.\n\
Choose create_new when the draft is materially distinct from every candidate.\n\
Choose reuse_existing when the existing candidate already covers the draft; the candidate should be returned unchanged.\n\
Choose merge_into_existing only when you have read the candidate's full rendered body and both the draft and that body contribute non-trivially to a combined note.\n\
Choose supersede_existing only when you have read the candidate's full rendered body and the draft is a strict improvement or replacement for that specific candidate.\n\
Do not choose merge or supersede unless the candidate's full body is present in the prompt; prefer create_new if you are uncertain.";

#[derive(Debug, serde::Deserialize)]
struct MemoryWriteDedupDecisionPayload {
    action: String,
    candidate_id: Option<String>,
    merged_title: Option<String>,
    merged_content: Option<String>,
    reason: Option<String>,
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
        "Decide whether to create a new note, reuse an existing candidate, merge into an existing candidate, or supersede an existing candidate. Respond with JSON only.",
    );

    prompt
}

fn format_candidate(candidate: &NoteDedupCandidate) -> String {
    let summary = candidate
        .overview
        .as_deref()
        .or(candidate.abstract_.as_deref())
        .unwrap_or("");
    let body = truncate_candidate_content(&candidate.content);

    format!(
        "- id: {}\n  title: {}\n  permalink: {}\n  score: {:.3}\n  body:\n{}\n  summary: {}",
        candidate.id, candidate.title, candidate.permalink, candidate.score, body, summary
    )
}

fn truncate_candidate_content(content: &str) -> String {
    let mut characters = content.chars();
    let body: String = characters
        .by_ref()
        .take(MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP)
        .collect();

    if characters.next().is_some() {
        format!(
            "{body}\n… [truncated at {MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP} characters]"
        )
    } else {
        body
    }
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
        "supersede_existing" => Ok(MemoryWriteDedupDecision::SupersedeExisting {
            candidate_id: payload
                .candidate_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "supersede_existing requires candidate_id".to_string())?,
            reason: payload
                .reason
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "supersede_existing requires reason".to_string())?,
        }),
        other => Err(format!("unknown dedup action: {other}")),
    }
}
