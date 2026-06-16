use std::collections::{BTreeSet, HashSet};

use serde::Serialize;

use super::{embedding_document_text, legacy_embedding_document_text};

const RANK_LIMIT: usize = 3;
const PROMPT_NOTE_LIMIT: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub struct ReplayFixture {
    pub name: &'static str,
    pub notes: Vec<ReplayNote>,
    pub queries: Vec<ReplayQuery>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayNote {
    pub id: &'static str,
    pub title: &'static str,
    pub note_type: &'static str,
    pub tags: &'static [&'static str],
    pub content: &'static str,
    pub retrieval_anchor: &'static str,
    pub injected_summary: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayQuery {
    pub id: &'static str,
    pub text: &'static str,
    pub relevant_note_ids: &'static [&'static str],
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReplayReport {
    pub fixture_name: String,
    pub passed: bool,
    pub criteria: ReplayCriteria,
    pub queries: Vec<QueryReplayReport>,
    pub prompt_budget: PromptBudgetReport,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReplayCriteria {
    pub rank_limit: usize,
    pub require_anchor_top_hit_relevant: bool,
    pub require_anchor_recall_at_limit_at_least_legacy: bool,
    pub require_anchor_mrr_at_least_legacy: bool,
    pub require_prompt_payload_chars_not_increased: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QueryReplayReport {
    pub query_id: String,
    pub query: String,
    pub relevant_note_ids: Vec<String>,
    pub legacy: RankingReport,
    pub anchor: RankingReport,
    pub passed: bool,
    pub failure_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RankingReport {
    pub ranking: Vec<RankedHit>,
    pub relevant_recall_at_limit: usize,
    pub reciprocal_rank: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RankedHit {
    pub rank: usize,
    pub note_id: String,
    pub title: String,
    pub score: f64,
    pub relevant: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PromptBudgetReport {
    pub legacy_selected_note_ids: Vec<String>,
    pub anchor_selected_note_ids: Vec<String>,
    pub legacy_payload_chars: usize,
    pub anchor_payload_chars: usize,
    pub delta_chars: isize,
    pub passed: bool,
    pub note: String,
}

#[derive(Debug, Clone, Copy)]
enum DocumentMode {
    LegacyFullContent,
    AnchorPreferred,
}

pub fn anchor_embedding_replay_fixture() -> ReplayFixture {
    ReplayFixture {
        name: "x72l-anchor-embedding-cutover-v1",
        notes: vec![
            ReplayNote {
                id: "case-review-feedback-loop",
                title: "Reviewer feedback loop converges with bounded local checks",
                note_type: "case",
                tags: &["review", "verification", "scope"],
                content: "A worker received reviewer feedback after a narrow Rust change. The successful follow-up read the activity log, made only the requested edits, ran the smallest focused cargo test, and submitted with remaining caveats. The body also mentions migrations, Kubernetes, Qdrant, prompt templates, OAuth, and graph extraction as unrelated examples that should not dominate retrieval.",
                retrieval_anchor: "When a worker resumes after reviewer or verification feedback and must make only the requested scoped fix.",
                injected_summary: "On reviewer/verification retry, inspect feedback first, patch only the requested scope, and use the narrowest local check before submit.",
            },
            ReplayNote {
                id: "pattern-parity-gate",
                title: "Parity gates need fixed fixtures and bounded reports",
                note_type: "pattern",
                tags: &["parity", "fixtures", "replay"],
                content: "A safe rollout gate compares an old artifact with a new artifact at the same revision. The report should include counts and bounded samples, avoid production traffic, and fail on unallowlisted drift. Examples include graph extraction, semantic retrieval, and prompt-policy replays.",
                retrieval_anchor: "When rolling out a semantic or graph behavior change that needs deterministic old-versus-new replay evidence.",
                injected_summary: "Use fixed fixtures, bounded diff output, and explicit pass/fail criteria for rollout parity gates; do not depend on live production traffic.",
            },
            ReplayNote {
                id: "pitfall-noisy-full-content",
                title: "Full-content embeddings can over-retrieve noisy implementation details",
                note_type: "pitfall",
                tags: &["embeddings", "noise", "memory"],
                content: "Long notes often contain incidental file names, stack traces, tool names, and unrelated examples. Embedding the whole body can match those incidental terms instead of the situation where the note applies. This pitfall appeared while discussing SQL migrations, Docker, OAuth, review loops, and graph snapshots in one document.",
                retrieval_anchor: "When memory search returns notes because of incidental body terms rather than the situation where the lesson applies.",
                injected_summary: "Prefer concise applicability anchors for embedding recall; full bodies can over-rank incidental implementation details.",
            },
            ReplayNote {
                id: "pattern-external-proof-runbook",
                title: "External proof belongs in runbooks, not worker acceptance",
                note_type: "pattern",
                tags: &["runbook", "external", "acceptance"],
                content: "If validation requires Docker daemons, production credentials, deployed clusters, or operator-only access, split that proof into a reviewable runbook. Worker acceptance should use local deterministic fixtures and document the unavailable external evidence separately.",
                retrieval_anchor: "When an acceptance criterion asks workers for production traffic, operator credentials, Docker, or cluster-only proof.",
                injected_summary: "Move unavailable external-infrastructure proof to a runbook and keep worker gates local, deterministic, and credential-free.",
            },
            ReplayNote {
                id: "case-prompt-budget",
                title: "Anchor retrieval does not expand session-start note injection",
                note_type: "case",
                tags: &["prompt", "budget", "anchors"],
                content: "The session prompt injects note summaries, abstracts, or overviews selected by retrieval. Retrieval anchors change the embedding document and hash used for selection, but the injected note body remains the existing summary payload rather than adding anchor text on top.",
                retrieval_anchor: "When measuring whether retrieval anchors increase the session-start knowledge-note prompt budget.",
                injected_summary: "Anchor text is used for retrieval selection and embedding hashes only; session-start note injection keeps using the existing note summary payload.",
            },
        ],
        queries: vec![
            ReplayQuery {
                id: "reviewer-retry-scope",
                text: "worker resumes after reviewer feedback verification failure scoped fix narrow cargo test",
                relevant_note_ids: &["case-review-feedback-loop"],
            },
            ReplayQuery {
                id: "rollout-parity-evidence",
                text: "deterministic replay gate old versus new semantic behavior fixed fixtures bounded report",
                relevant_note_ids: &["pattern-parity-gate"],
            },
            ReplayQuery {
                id: "embedding-noise",
                text: "memory search over retrieves incidental body terms instead of applicability situation",
                relevant_note_ids: &["pitfall-noisy-full-content"],
            },
            ReplayQuery {
                id: "external-infra-proof",
                text: "acceptance criterion requires production traffic operator credentials docker cluster proof",
                relevant_note_ids: &["pattern-external-proof-runbook"],
            },
            ReplayQuery {
                id: "prompt-budget-impact",
                text: "measure session start injected note payload prompt budget after retrieval anchors",
                relevant_note_ids: &["case-prompt-budget"],
            },
        ],
    }
}

pub fn generate_anchor_embedding_replay_report() -> ReplayReport {
    generate_replay_report(&anchor_embedding_replay_fixture())
}

pub fn render_anchor_embedding_replay_report_markdown(report: &ReplayReport) -> String {
    let mut output = String::new();
    output.push_str("# Anchor embedding deterministic replay report\n\n");
    output.push_str(&format!("Fixture: `{}`\n\n", report.fixture_name));
    output.push_str(&format!(
        "Overall result: **{}**\n\n",
        if report.passed { "PASS" } else { "FAIL" }
    ));
    output.push_str("## Non-regression criteria\n\n");
    output.push_str(&format!(
        "- Top anchor hit must be relevant: `{}`\n- Anchor recall@{} must be >= legacy recall@{} for every query: `{}`\n- Anchor reciprocal rank must be >= legacy reciprocal rank for every query: `{}`\n- Session-start injected note payload chars must not increase: `{}`\n\n",
        report.criteria.require_anchor_top_hit_relevant,
        report.criteria.rank_limit,
        report.criteria.rank_limit,
        report.criteria.require_anchor_recall_at_limit_at_least_legacy,
        report.criteria.require_anchor_mrr_at_least_legacy,
        report.criteria.require_prompt_payload_chars_not_increased,
    ));
    output.push_str("## Query replay\n\n");
    output.push_str(
        "| Query | Legacy top hits | Anchor top hits | Legacy RR | Anchor RR | Result |\n",
    );
    output.push_str("| --- | --- | --- | ---: | ---: | --- |\n");
    for query in &report.queries {
        output.push_str(&format!(
            "| `{}` | {} | {} | {:.3} | {:.3} | {} |\n",
            query.query_id,
            render_hits_inline(&query.legacy.ranking),
            render_hits_inline(&query.anchor.ranking),
            query.legacy.reciprocal_rank,
            query.anchor.reciprocal_rank,
            if query.passed { "PASS" } else { "FAIL" },
        ));
    }
    output.push_str("\n## Prompt-budget measurement\n\n");
    output.push_str(&format!(
        "- Legacy selected note ids: `{}`\n- Anchor selected note ids: `{}`\n- Legacy rendered payload chars: `{}`\n- Anchor rendered payload chars: `{}`\n- Delta chars: `{}`\n- Result: **{}**\n\n{}\n",
        report.prompt_budget.legacy_selected_note_ids.join(", "),
        report.prompt_budget.anchor_selected_note_ids.join(", "),
        report.prompt_budget.legacy_payload_chars,
        report.prompt_budget.anchor_payload_chars,
        report.prompt_budget.delta_chars,
        if report.prompt_budget.passed { "PASS" } else { "FAIL" },
        report.prompt_budget.note,
    ));
    output
}

fn generate_replay_report(fixture: &ReplayFixture) -> ReplayReport {
    let criteria = ReplayCriteria {
        rank_limit: RANK_LIMIT,
        require_anchor_top_hit_relevant: true,
        require_anchor_recall_at_limit_at_least_legacy: true,
        require_anchor_mrr_at_least_legacy: true,
        require_prompt_payload_chars_not_increased: true,
    };

    let mut query_reports = Vec::new();
    for query in &fixture.queries {
        let legacy = rank_query(fixture, query, DocumentMode::LegacyFullContent);
        let anchor = rank_query(fixture, query, DocumentMode::AnchorPreferred);
        let mut failure_reasons = Vec::new();
        if criteria.require_anchor_top_hit_relevant
            && !anchor.ranking.first().is_some_and(|hit| hit.relevant)
        {
            failure_reasons.push("anchor top hit is not relevant".to_string());
        }
        if criteria.require_anchor_recall_at_limit_at_least_legacy
            && anchor.relevant_recall_at_limit < legacy.relevant_recall_at_limit
        {
            failure_reasons.push(format!(
                "anchor recall@{} ({}) is below legacy recall@{} ({})",
                criteria.rank_limit,
                anchor.relevant_recall_at_limit,
                criteria.rank_limit,
                legacy.relevant_recall_at_limit
            ));
        }
        if criteria.require_anchor_mrr_at_least_legacy
            && anchor.reciprocal_rank + f64::EPSILON < legacy.reciprocal_rank
        {
            failure_reasons.push(format!(
                "anchor reciprocal rank ({:.3}) is below legacy reciprocal rank ({:.3})",
                anchor.reciprocal_rank, legacy.reciprocal_rank
            ));
        }

        query_reports.push(QueryReplayReport {
            query_id: query.id.to_string(),
            query: query.text.to_string(),
            relevant_note_ids: query
                .relevant_note_ids
                .iter()
                .map(|id| (*id).to_string())
                .collect(),
            legacy,
            anchor,
            passed: failure_reasons.is_empty(),
            failure_reasons,
        });
    }

    let prompt_budget = measure_prompt_budget(fixture, &query_reports);
    let passed = query_reports.iter().all(|query| query.passed) && prompt_budget.passed;
    ReplayReport {
        fixture_name: fixture.name.to_string(),
        passed,
        criteria,
        queries: query_reports,
        prompt_budget,
    }
}

fn rank_query(fixture: &ReplayFixture, query: &ReplayQuery, mode: DocumentMode) -> RankingReport {
    let relevant: HashSet<&str> = query.relevant_note_ids.iter().copied().collect();
    let query_tokens = tokenize(query.text);
    let mut scored = fixture
        .notes
        .iter()
        .map(|note| {
            let document = match mode {
                DocumentMode::LegacyFullContent => legacy_embedding_document_text(
                    note.title,
                    note.note_type,
                    &serde_json::to_string(note.tags).expect("serialize tags"),
                    note.content,
                ),
                DocumentMode::AnchorPreferred => embedding_document_text(
                    note.title,
                    note.note_type,
                    &serde_json::to_string(note.tags).expect("serialize tags"),
                    note.content,
                    Some(note.retrieval_anchor),
                ),
            };
            let score = score_tokens(&query_tokens, &tokenize(&document));
            (note, score)
        })
        .collect::<Vec<_>>();

    scored.sort_by(|(left_note, left_score), (right_note, right_score)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| left_note.id.cmp(right_note.id))
    });

    let ranking = scored
        .into_iter()
        .take(RANK_LIMIT)
        .enumerate()
        .map(|(idx, (note, score))| RankedHit {
            rank: idx + 1,
            note_id: note.id.to_string(),
            title: note.title.to_string(),
            score,
            relevant: relevant.contains(note.id),
        })
        .collect::<Vec<_>>();

    let relevant_recall_at_limit = ranking.iter().filter(|hit| hit.relevant).count();
    let reciprocal_rank = ranking
        .iter()
        .find(|hit| hit.relevant)
        .map(|hit| 1.0 / hit.rank as f64)
        .unwrap_or(0.0);

    RankingReport {
        ranking,
        relevant_recall_at_limit,
        reciprocal_rank,
    }
}

fn tokenize(input: &str) -> BTreeSet<String> {
    input
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|token| {
            let token = token.trim().to_ascii_lowercase();
            (token.len() >= 3).then_some(token)
        })
        .collect()
}

fn score_tokens(query: &BTreeSet<String>, document: &BTreeSet<String>) -> f64 {
    if query.is_empty() || document.is_empty() {
        return 0.0;
    }
    let overlap = query.intersection(document).count() as f64;
    overlap / (query.len() as f64).sqrt() / (document.len() as f64).sqrt()
}

fn measure_prompt_budget(
    fixture: &ReplayFixture,
    query_reports: &[QueryReplayReport],
) -> PromptBudgetReport {
    let legacy_ids = selected_note_ids(query_reports, |query| &query.legacy.ranking);
    let anchor_ids = selected_note_ids(query_reports, |query| &query.anchor.ranking);
    let legacy_payload = render_session_start_note_payload(fixture, &legacy_ids);
    let anchor_payload = render_session_start_note_payload(fixture, &anchor_ids);
    let legacy_payload_chars = legacy_payload.chars().count();
    let anchor_payload_chars = anchor_payload.chars().count();
    let delta_chars = anchor_payload_chars as isize - legacy_payload_chars as isize;
    let passed = delta_chars <= 0;
    PromptBudgetReport {
        legacy_selected_note_ids: legacy_ids,
        anchor_selected_note_ids: anchor_ids,
        legacy_payload_chars,
        anchor_payload_chars,
        delta_chars,
        passed,
        note: "Measured payload renders the same session-start note bodies/summaries used after retrieval selection. Retrieval anchors change only embedding document text and hashes; they are not appended to injected note bodies.".to_string(),
    }
}

fn selected_note_ids<F>(query_reports: &[QueryReplayReport], select: F) -> Vec<String>
where
    F: Fn(&QueryReplayReport) -> &[RankedHit],
{
    let mut ids = Vec::new();
    for query in query_reports {
        for hit in select(query) {
            if !ids.iter().any(|existing| existing == &hit.note_id) {
                ids.push(hit.note_id.clone());
            }
            if ids.len() >= PROMPT_NOTE_LIMIT {
                return ids;
            }
        }
    }
    ids
}

fn render_session_start_note_payload(fixture: &ReplayFixture, note_ids: &[String]) -> String {
    let mut lines = Vec::new();
    for note_id in note_ids {
        if let Some(note) = fixture.notes.iter().find(|note| note.id == note_id) {
            let label = match note.note_type {
                "pitfall" => "Pitfall",
                "pattern" => "Pattern",
                "case" => "Case",
                _ => "Note",
            };
            lines.push(format!(
                "- **[{}] {}**: {}",
                label, note.title, note.injected_summary
            ));
        }
    }
    lines.join("\n")
}

fn render_hits_inline(hits: &[RankedHit]) -> String {
    hits.iter()
        .map(|hit| {
            let marker = if hit.relevant { "*" } else { "" };
            format!("{}{}:{:.3}", marker, hit.note_id, hit.score)
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_embedding_replay_report_passes_non_regression_gate() {
        let report = generate_anchor_embedding_replay_report();

        assert!(report.passed, "{:#?}", report);
        assert_eq!(report.queries.len(), 5);
        for query in &report.queries {
            assert!(query.passed, "{:#?}", query);
            assert!(query.anchor.ranking.first().is_some_and(|hit| hit.relevant));
            assert!(
                query.anchor.relevant_recall_at_limit >= query.legacy.relevant_recall_at_limit,
                "{:#?}",
                query
            );
            assert!(
                query.anchor.reciprocal_rank + f64::EPSILON >= query.legacy.reciprocal_rank,
                "{:#?}",
                query
            );
            assert!(query.anchor.ranking.len() <= RANK_LIMIT);
            assert!(query.legacy.ranking.len() <= RANK_LIMIT);
        }
    }

    #[test]
    fn replay_report_markdown_is_bounded_and_reviewable() {
        let report = generate_anchor_embedding_replay_report();
        let markdown = render_anchor_embedding_replay_report_markdown(&report);

        assert!(markdown.contains("Overall result: **PASS**"));
        assert!(markdown.contains("| `rollout-parity-evidence` |"));
        assert!(markdown.contains("## Prompt-budget measurement"));
        assert!(markdown.len() < 8_000, "report should stay bounded");
    }

    #[test]
    fn prompt_budget_payload_does_not_expand_with_anchor_flow() {
        let report = generate_anchor_embedding_replay_report();

        assert!(report.prompt_budget.passed, "{:#?}", report.prompt_budget);
        assert!(report.prompt_budget.delta_chars <= 0);
        assert_eq!(
            report.prompt_budget.anchor_payload_chars, report.prompt_budget.legacy_payload_chars,
            "fixture expects unchanged injected note bodies; anchors only affect retrieval documents"
        );
    }
}
