// MCP-owned note summary helpers and worker implementation.

use djinn_core::events::EventBus;
use djinn_db::{Database, NoteRepository};
use djinn_provider::{
    CompletionRequest, complete, prompts::MEMORY_L0_ABSTRACT, prompts::MEMORY_L1_OVERVIEW,
    provider::LlmProvider, resolve_memory_provider,
};
use tracing::warn;

const LLM_MAX_TOKENS: u32 = 512;
const L0_TOKEN_LIMIT: usize = 100;
const L1_TOKEN_LIMIT: usize = 500;
const SUMMARY_SYSTEM: &str = "You are an expert note summarizer.";

/// MCP-owned worker that generates and persists L0/L1 note summaries.
pub struct NoteSummaryService {
    db: Database,
}

impl NoteSummaryService {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Generate summaries for the provided note IDs.
    ///
    /// Failures are logged and swallowed by design; the caller path should remain
    /// successful even when generation, persistence, or provider resolution fails.
    pub async fn generate_for_note_ids(&self, note_ids: &[String]) {
        let repo = NoteRepository::new(self.db.clone(), EventBus::noop());

        let provider = match resolve_memory_provider(&self.db).await {
            Ok(provider) => Some(provider),
            Err(error) => {
                warn!(error = %error, "note summary generation: provider unavailable, using fallback summaries");
                None
            }
        };
        let provider = provider.as_deref();

        for note_id in note_ids {
            self.generate_for_note(&repo, note_id.as_str(), provider)
                .await;
        }
    }

    async fn generate_for_note(
        &self,
        repo: &NoteRepository,
        note_id: &str,
        provider: Option<&dyn LlmProvider>,
    ) {
        let note = match repo.get(note_id).await {
            Ok(Some(note)) => note,
            Ok(None) => {
                warn!(note_id = %note_id, "note summary generation: note not found");
                return;
            }
            Err(error) => {
                warn!(note_id = %note_id, error = %error, "note summary generation: failed to load note");
                return;
            }
        };

        let abstract_prompt = render_memory_prompt(MEMORY_L0_ABSTRACT, &note.title, &note.content);
        let overview_prompt = render_memory_prompt(MEMORY_L1_OVERVIEW, &note.title, &note.content);

        let abstract_summary =
            complete_summary(provider, SUMMARY_SYSTEM, &abstract_prompt, LLM_MAX_TOKENS)
                .await
                .unwrap_or_else(|| fallback_l0_summary(&note.content, L0_TOKEN_LIMIT));

        let overview_summary =
            complete_summary(provider, SUMMARY_SYSTEM, &overview_prompt, LLM_MAX_TOKENS)
                .await
                .unwrap_or_else(|| fallback_l1_summary(&note.content, L1_TOKEN_LIMIT));

        if let Err(error) = repo
            .update_summaries(
                &note.id,
                Some(abstract_summary.as_str()),
                Some(overview_summary.as_str()),
            )
            .await
        {
            warn!(
                note_id = %note_id,
                error = %error,
                "note summary generation: failed to persist summaries"
            );
        }
    }
}

fn render_memory_prompt(template: &str, title: &str, content: &str) -> String {
    template
        .replace("{{title}}", title)
        .replace("{{content}}", content)
}

async fn complete_summary(
    provider: Option<&dyn LlmProvider>,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Option<String> {
    let provider = provider?;

    match complete(
        provider,
        CompletionRequest {
            system: system.to_string(),
            prompt: prompt.to_string(),
            max_tokens,
        },
    )
    .await
    {
        Ok(response) => {
            let text = response.text.trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        }
        Err(error) => {
            warn!(error = %error, "note summary generation: completion failed");
            None
        }
    }
}

fn estimate_tokens(value: &str) -> usize {
    value
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .count()
}

fn fallback_l0_summary(content: &str, max_tokens: usize) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(sentence) = first_complete_sentence(trimmed) {
        let sentence = sentence.trim();
        return truncate_to_token_limit(sentence, max_tokens);
    }

    truncate_to_token_limit(trimmed, max_tokens)
}

fn fallback_l1_summary(content: &str, max_tokens: usize) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if estimate_tokens(trimmed) <= max_tokens {
        return trimmed.to_string();
    }

    let paragraphs = split_paragraphs(trimmed);
    if paragraphs.len() <= 1 {
        return truncate_to_token_limit(trimmed, max_tokens);
    }

    let (head_budget, tail_budget) = l1_budgets(max_tokens);

    let mut head_indices = Vec::new();
    let mut head_used = 0usize;
    for (index, paragraph) in paragraphs.iter().enumerate() {
        let tokens = estimate_tokens(paragraph);

        if head_used == 0 && tokens > head_budget && head_budget > 0 {
            head_indices.push(index);
            break;
        }

        if head_used + tokens <= head_budget {
            head_indices.push(index);
            head_used += tokens;
            continue;
        }

        break;
    }

    let mut tail_indices = Vec::new();
    let mut tail_used = 0usize;
    for index in (0..paragraphs.len()).rev() {
        let paragraph = &paragraphs[index];
        let tokens = estimate_tokens(paragraph);

        if tail_used == 0 && tokens > tail_budget && tail_budget > 0 {
            tail_indices.push(index);
            break;
        }

        if tail_used + tokens <= tail_budget {
            tail_indices.push(index);
            tail_used += tokens;
            continue;
        }

        break;
    }

    tail_indices.sort_unstable();

    let mut selected_indices = Vec::with_capacity(head_indices.len() + tail_indices.len());
    for index in head_indices {
        if !selected_indices.contains(&index) {
            selected_indices.push(index);
        }
    }
    for index in tail_indices {
        if !selected_indices.contains(&index) {
            selected_indices.push(index);
        }
    }
    selected_indices.sort_unstable();

    join_selected_paragraphs(&paragraphs, &selected_indices, max_tokens)
}

fn l1_budgets(max_tokens: usize) -> (usize, usize) {
    if max_tokens == 0 {
        return (0, 0);
    }

    let head_budget = max_tokens.saturating_mul(60) / 100;
    let tail_budget = max_tokens.saturating_sub(head_budget);
    (head_budget, tail_budget)
}

fn split_paragraphs(content: &str) -> Vec<String> {
    content
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| !paragraph.is_empty())
        .map(|paragraph| paragraph.to_string())
        .collect()
}

fn join_selected_paragraphs(paragraphs: &[String], indices: &[usize], max_tokens: usize) -> String {
    if indices.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    let mut previous: Option<usize> = None;
    for &index in indices {
        if previous.is_some() {
            result.push_str("\n\n");
        }

        result.push_str(&paragraphs[index]);
        previous = Some(index);
    }

    if estimate_tokens(&result) <= max_tokens {
        return result;
    }

    truncate_to_token_limit(&result, max_tokens)
}

fn first_complete_sentence(content: &str) -> Option<&str> {
    for (index, ch) in content.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            return Some(&content[..index + ch.len_utf8()]);
        }
    }
    None
}

fn truncate_to_token_limit(content: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }

    let words: Vec<&str> = content.split_whitespace().collect();
    if words.len() <= max_tokens {
        return words.join(" ");
    }

    words[..max_tokens].join(" ")
}

#[cfg(test)]
mod tests {
    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{Database, NoteRepository, ProjectRepository};
    use tokio::sync::broadcast;

    use super::*;

    fn event_bus_for(tx: &broadcast::Sender<DjinnEventEnvelope>) -> EventBus {
        let tx = tx.clone();
        EventBus::new(move |event| {
            let _ = tx.send(event);
        })
    }

    async fn make_project(db: &Database, path: &std::path::Path) -> djinn_core::models::Project {
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("test-project", path.to_str().unwrap())
            .await
            .unwrap()
    }

    async fn make_note(
        repo: &NoteRepository,
        project_id: &str,
        path: &std::path::Path,
        title: &str,
        content: &str,
    ) -> djinn_core::models::Note {
        repo.create(project_id, path, title, content, "reference", "[]")
            .await
            .unwrap()
    }

    #[test]
    fn render_memory_prompt_replaces_fields() {
        let rendered = render_memory_prompt(MEMORY_L0_ABSTRACT, "My Note", "hello world");
        assert!(!rendered.contains("{{title}}"));
        assert!(!rendered.contains("{{content}}"));
        assert!(rendered.contains("My Note"));
        assert!(rendered.contains("hello world"));
    }

    #[test]
    fn fallback_l0_uses_first_complete_sentence() {
        let content = "This is sentence one. This is sentence two.";
        assert_eq!(fallback_l0_summary(content, 100), "This is sentence one.");
    }

    #[test]
    fn fallback_l0_truncates_sentence_when_limit_too_small() {
        let content = "one two three four five six. second sentence follows";
        assert_eq!(fallback_l0_summary(content, 3), "one two three");
    }

    #[test]
    fn fallback_l1_keeps_head_and_tail_paragraphs() {
        let content = [
            "paragraph one content",
            "paragraph two content",
            "paragraph three",
            "paragraph four",
            "paragraph five",
        ]
        .join("\n\n");

        let summary = fallback_l1_summary(&content, 6);

        assert!(summary.contains("paragraph one content"));
        assert!(summary.contains("paragraph five"));
        assert!(!summary.contains("paragraph three"));
        assert!(!summary.contains("paragraph four"));
        assert!(!summary.contains("paragraph two content"));
        assert!(estimate_tokens(&summary) <= 6);
    }

    #[test]
    fn l1_budget_keeps_60_40_ratio() {
        let (head, tail) = l1_budgets(10);
        assert_eq!(head, 6);
        assert_eq!(tail, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generate_for_note_ids_persists_summaries_without_bubbling() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);

        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let note = make_note(
            &repo,
            &project.id,
            tmp.path(),
            "Summary Note",
            "Sentence one. Sentence two. sentence three.\n\nParagraph B.\n\nParagraph C.",
        )
        .await;

        let service = NoteSummaryService::new(db.clone());
        service
            .generate_for_note_ids(std::slice::from_ref(&note.id))
            .await;

        let updated = repo.get(&note.id).await.unwrap().unwrap();
        let abstract_summary = updated
            .abstract_
            .expect("expected abstract summary to be persisted");
        let overview_summary = updated
            .overview
            .expect("expected overview summary to be persisted");

        assert!(!abstract_summary.is_empty());
        assert!(!overview_summary.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generate_for_note_ids_unknown_ids_are_safely_ignored() {
        let db = Database::open_in_memory().unwrap();
        let service = NoteSummaryService::new(db);

        // no notes exist, no provider configured, and no panic should occur.
        service
            .generate_for_note_ids(&["missing-note-id".to_string()])
            .await;
    }
}
