// MCP-owned note summary helpers and worker implementation.

use std::time::Duration;

use djinn_core::events::EventBus;
use djinn_db::{Database, NoteRepository};
use djinn_provider::{
    CompletionRequest, complete, prompts::MEMORY_L0_ABSTRACT,
    prompts::MEMORY_L0_APPLICABILITY_PREFIX, prompts::MEMORY_L1_OVERVIEW, provider::LlmProvider,
    resolve_memory_provider_for_user,
};
use sqlx::Row;
use tokio::sync::mpsc;
use tracing::{info, warn};

const LLM_MAX_TOKENS: u32 = 512;
const L0_TOKEN_LIMIT: usize = 100;
const L1_TOKEN_LIMIT: usize = 500;
const SUMMARY_SYSTEM: &str = "You are an expert note summarizer.";
pub(crate) const SUMMARY_BACKFILL_QUEUE_CAPACITY: usize = 32;
pub(crate) const SUMMARY_BACKFILL_DELAY: Duration = Duration::from_millis(100);

/// MCP-owned worker that generates and persists L0/L1 note summaries.
pub struct NoteSummaryService {
    db: Database,
    user_id: Option<String>,
}

impl NoteSummaryService {
    pub fn new(db: Database) -> Self {
        Self::new_for_user(db, None)
    }

    pub fn new_for_user(db: Database, user_id: Option<String>) -> Self {
        Self { db, user_id }
    }

    /// Generate summaries for the provided note IDs.
    ///
    /// Failures are logged and swallowed by design; the caller path should remain
    /// successful even when generation, persistence, or provider resolution fails.
    pub async fn generate_for_note_ids(&self, note_ids: &[String]) {
        let repo = NoteRepository::new(self.db.clone(), EventBus::noop());

        let provider = match resolve_memory_provider_for_user(&self.db, self.user_id.as_deref())
            .await
        {
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

    pub async fn apply_fallback_for_note_id(&self, note_id: &str) {
        let repo = NoteRepository::new(self.db.clone(), EventBus::noop());
        self.apply_fallback_for_note(&repo, note_id).await;
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

    async fn apply_fallback_for_note(&self, repo: &NoteRepository, note_id: &str) {
        let note = match repo.get(note_id).await {
            Ok(Some(note)) => note,
            Ok(None) => {
                warn!(note_id = %note_id, "note summary fallback: note not found");
                return;
            }
            Err(error) => {
                warn!(note_id = %note_id, error = %error, "note summary fallback: failed to load note");
                return;
            }
        };

        let abstract_summary = fallback_l0_summary(&note.content, L0_TOKEN_LIMIT);
        let overview_summary = fallback_l1_summary(&note.content, L1_TOKEN_LIMIT);

        if let Err(error) = repo
            .update_summaries(
                &note.id,
                Some(abstract_summary.as_str()),
                Some(overview_summary.as_str()),
            )
            .await
        {
            warn!(note_id = %note_id, error = %error, "note summary fallback: failed to persist summaries");
        }
    }
}

// ── Abstract vintage: coverage counter ────────────────────────────────────────

/// SQL `ILIKE` pattern that matches an abstract written by the current
/// applicability-condition L0 prompt.
fn current_vintage_pattern() -> String {
    format!("{MEMORY_L0_APPLICABILITY_PREFIX}%")
}

/// How much of the corpus carries an abstract from the *current* L0 prompt.
///
/// The transition between prompt vintages is the risk the u46i proposal flags:
/// for as long as it runs, ranking sees a mix of applicability-condition
/// abstracts and pre-u46i one-liners. This makes the mix countable instead of
/// invisible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AbstractVintageCoverage {
    /// Notes in scope.
    pub total: i64,
    /// Notes with any non-empty abstract.
    pub with_abstract: i64,
    /// Notes whose abstract opens with [`MEMORY_L0_APPLICABILITY_PREFIX`].
    pub current_vintage: i64,
}

impl AbstractVintageCoverage {
    /// Notes carrying an abstract from a prompt older than u46i.
    pub fn legacy_vintage(&self) -> i64 {
        self.with_abstract.saturating_sub(self.current_vintage)
    }

    /// Notes with no usable abstract at all.
    pub fn missing(&self) -> i64 {
        self.total.saturating_sub(self.with_abstract)
    }

    /// Fraction of in-scope notes on the current prompt, in `0.0..=1.0`.
    pub fn current_ratio(&self) -> f64 {
        if self.total <= 0 {
            return 0.0;
        }
        self.current_vintage as f64 / self.total as f64
    }

    /// True when nothing is left to regenerate.
    pub fn is_converged(&self) -> bool {
        self.total > 0 && self.current_vintage == self.total
    }
}

/// Count abstract vintages across the corpus, optionally narrowed to note types.
///
/// An empty `note_types` means "every type". Scoped to `status = 'active'`,
/// matching [`select_stale_abstract_note_ids`] exactly — if the counter measured
/// a wider population than the sweep can act on, [`AbstractVintageCoverage::is_converged`]
/// could never become true and the metric would look permanently stuck.
pub async fn abstract_vintage_coverage(
    db: &Database,
    note_types: &[String],
) -> Result<AbstractVintageCoverage, sqlx::Error> {
    let types: Vec<String> = note_types.to_vec();
    let row = sqlx::query(
        r#"SELECT
               count(*) AS total,
               count(*) FILTER (
                   WHERE abstract IS NOT NULL AND btrim(abstract) <> ''
               ) AS with_abstract,
               count(*) FILTER (
                   WHERE abstract IS NOT NULL AND btrim(abstract) ILIKE $2
               ) AS current_vintage
           FROM notes
           WHERE (cardinality($1::text[]) = 0 OR note_type = ANY($1::text[]))
             AND status = 'active'"#,
    )
    .bind(&types)
    .bind(current_vintage_pattern())
    .fetch_one(db.pool())
    .await?;

    Ok(AbstractVintageCoverage {
        total: row.try_get::<i64, _>("total")?,
        with_abstract: row.try_get::<i64, _>("with_abstract")?,
        current_vintage: row.try_get::<i64, _>("current_vintage")?,
    })
}

// ── Abstract vintage: the sweep ───────────────────────────────────────────────

pub(crate) const SWEEP_DEFAULT_BATCH_SIZE: i64 = 200;

/// Environment switches for the startup sweep. Absent/unset means **off**.
pub(crate) const SWEEP_ENABLE_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP";
pub(crate) const SWEEP_BATCH_SIZE_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP_BATCH_SIZE";
pub(crate) const SWEEP_MAX_NOTES_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP_MAX_NOTES";
pub(crate) const SWEEP_TYPES_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP_TYPES";

/// Bounds for one sweep run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbstractSweepConfig {
    /// Rows read per keyset page.
    pub batch_size: i64,
    /// Hard cap on notes enqueued in a single run. `None` means "the whole
    /// remaining corpus", which is ~10.7k notes for the injected types.
    pub max_notes: Option<usize>,
    /// Restrict to these `note_type` values. Empty means every type.
    pub note_types: Vec<String>,
}

impl Default for AbstractSweepConfig {
    fn default() -> Self {
        Self {
            batch_size: SWEEP_DEFAULT_BATCH_SIZE,
            max_notes: None,
            note_types: Vec::new(),
        }
    }
}

/// What one sweep run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AbstractSweepReport {
    /// Keyset pages read.
    pub batches: usize,
    /// Stale notes the selection returned.
    pub selected: usize,
    /// Notes handed to the summary worker.
    pub enqueued: usize,
    /// Last note id considered — where a follow-up run would resume within the
    /// same pass. Persisting it is *not* required for correctness: the selection
    /// predicate already excludes anything regenerated.
    pub cursor: Option<String>,
    /// Run stopped because `max_notes` was hit, not because work ran out.
    pub reached_cap: bool,
    /// Run stopped because the worker's receiver was gone.
    pub sink_closed: bool,
}

/// Page of note ids whose abstract does not come from the current L0 prompt.
///
/// Keyset-paginated on `id`. The predicate is what makes the sweep idempotent:
/// a note drops out of the result set the moment its abstract is rewritten, so
/// a restarted sweep never redoes finished work. The cursor is what makes a
/// single pass terminate: without it a note the provider failed on would be
/// re-selected forever.
///
/// Restricted to `status = 'active'`: regenerating an abstract costs a live LLM
/// call, and a note a human archived is not one worth spending on.
pub async fn select_stale_abstract_note_ids(
    db: &Database,
    after_id: Option<&str>,
    limit: i64,
    note_types: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    let types: Vec<String> = note_types.to_vec();
    sqlx::query_scalar::<_, String>(
        r#"SELECT id
           FROM notes
           WHERE ($1::text IS NULL OR id > $1::text)
             AND (cardinality($2::text[]) = 0 OR note_type = ANY($2::text[]))
             AND status = 'active'
             AND (
                 abstract IS NULL
                 OR btrim(abstract) = ''
                 OR btrim(abstract) NOT ILIKE $3
             )
           ORDER BY id
           LIMIT $4"#,
    )
    .bind(after_id)
    .bind(&types)
    .bind(current_vintage_pattern())
    .bind(limit.max(1))
    .fetch_all(db.pool())
    .await
}

/// Enumerate stale abstracts and feed them to the existing summary worker.
///
/// This deliberately owns no generation logic: it pushes note ids into the same
/// [`spawn_summary_backfill_worker`] channel the read-triggered path uses, so
/// there is exactly one place that talks to a provider. `send().await` on a
/// bounded channel is also the rate limiter — the sweep cannot outrun the
/// worker's `SUMMARY_BACKFILL_DELAY`, so it cannot stampede the provider no
/// matter how large the corpus is.
pub async fn run_abstract_sweep(
    db: &Database,
    sink: &mpsc::Sender<String>,
    config: &AbstractSweepConfig,
) -> Result<AbstractSweepReport, sqlx::Error> {
    let mut report = AbstractSweepReport::default();
    let mut cursor: Option<String> = None;

    loop {
        if let Some(max) = config.max_notes
            && report.enqueued >= max
        {
            report.reached_cap = true;
            break;
        }

        let remaining = config
            .max_notes
            .map(|max| max.saturating_sub(report.enqueued) as i64)
            .unwrap_or(config.batch_size);
        let limit = config.batch_size.min(remaining.max(1));

        let page = select_stale_abstract_note_ids(db, cursor.as_deref(), limit, &config.note_types)
            .await?;
        if page.is_empty() {
            break;
        }

        report.batches += 1;
        report.selected += page.len();

        for note_id in page {
            cursor = Some(note_id.clone());
            report.cursor = cursor.clone();

            if sink.send(note_id).await.is_err() {
                report.sink_closed = true;
                return Ok(report);
            }
            report.enqueued += 1;

            if let Some(max) = config.max_notes
                && report.enqueued >= max
            {
                report.reached_cap = true;
                return Ok(report);
            }
        }
    }

    Ok(report)
}

/// Parse the sweep configuration from an environment lookup.
///
/// Returns `None` unless [`SWEEP_ENABLE_ENV`] is explicitly truthy. The sweep is
/// off by default on purpose: a full pass is ~10.7k live LLM calls, real spend,
/// and hours of wall time, so it must be an operator decision and never a side
/// effect of a deploy.
pub(crate) fn sweep_config_from_env<F>(lookup: F) -> Option<AbstractSweepConfig>
where
    F: Fn(&str) -> Option<String>,
{
    let enabled = lookup(SWEEP_ENABLE_ENV)?;
    if !matches!(
        enabled.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    ) {
        return None;
    }

    let batch_size = lookup(SWEEP_BATCH_SIZE_ENV)
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(SWEEP_DEFAULT_BATCH_SIZE);

    let max_notes = lookup(SWEEP_MAX_NOTES_ENV)
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0);

    let note_types = lookup(SWEEP_TYPES_ENV)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(AbstractSweepConfig {
        batch_size,
        max_notes,
        note_types,
    })
}

fn log_abstract_coverage(coverage: AbstractVintageCoverage, phase: &'static str) {
    info!(
        phase,
        total = coverage.total,
        current_vintage = coverage.current_vintage,
        legacy_vintage = coverage.legacy_vintage(),
        missing = coverage.missing(),
        current_ratio = coverage.current_ratio(),
        converged = coverage.is_converged(),
        "memory L0 abstract vintage coverage"
    );
}

pub(crate) fn spawn_summary_backfill_worker(db: Database) -> mpsc::Sender<String> {
    let (tx, mut rx) = mpsc::channel(SUMMARY_BACKFILL_QUEUE_CAPACITY);

    let sweep_config = sweep_config_from_env(|key| std::env::var(key).ok());
    let observer_db = db.clone();
    let observer_tx = tx.clone();
    tokio::spawn(async move {
        // Always report the vintage mix, sweep or no sweep: the mixed-vintage
        // window is precisely the interval where the sweep is *not* running.
        let note_types = sweep_config
            .as_ref()
            .map(|config| config.note_types.clone())
            .unwrap_or_default();

        match abstract_vintage_coverage(&observer_db, &note_types).await {
            Ok(coverage) => log_abstract_coverage(coverage, "startup"),
            Err(error) => {
                warn!(error = %error, "abstract vintage coverage: query failed");
            }
        }

        let Some(config) = sweep_config else {
            return;
        };

        info!(
            batch_size = config.batch_size,
            max_notes = ?config.max_notes,
            note_types = ?config.note_types,
            "memory L0 abstract sweep enabled; enqueuing stale abstracts"
        );

        match run_abstract_sweep(&observer_db, &observer_tx, &config).await {
            Ok(report) => {
                info!(
                    batches = report.batches,
                    selected = report.selected,
                    enqueued = report.enqueued,
                    reached_cap = report.reached_cap,
                    sink_closed = report.sink_closed,
                    cursor = ?report.cursor,
                    "memory L0 abstract sweep finished enqueuing"
                );
            }
            Err(error) => {
                warn!(error = %error, "memory L0 abstract sweep: selection failed");
            }
        }
    });

    tokio::spawn(async move {
        // Backfill is a process-owned background worker: no request user exists,
        // so memory provider resolution is intentionally org-shared only.
        let service = NoteSummaryService::new_for_user(db, None);
        while let Some(note_id) = rx.recv().await {
            service.generate_for_note_ids(&[note_id]).await;
            tokio::time::sleep(SUMMARY_BACKFILL_DELAY).await;
        }
    });
    tx
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

    fn workspace_tempdir() -> tempfile::TempDir {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create server crate test tempdir base");
        tempfile::tempdir_in(base).expect("create server crate tempdir")
    }
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

    async fn make_project(db: &Database, _path: &std::path::Path) -> djinn_core::models::Project {
        db.ensure_initialized().await.unwrap();
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("test-project", "test", "test-project")
            .await
            .unwrap()
    }

    async fn make_note(
        repo: &NoteRepository,
        project_id: &str,
        _path: &std::path::Path,
        title: &str,
        content: &str,
    ) -> djinn_memory::Note {
        repo.create(project_id, title, content, "reference", "[]")
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
        let tmp = workspace_tempdir();
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

    // ── Abstract vintage sweep ────────────────────────────────────────────────
    //
    // Every test below drives selection/batching/resumability against a real
    // ephemeral Postgres and a plain `mpsc` sink. No provider is resolved and no
    // completion is issued: executing the ~13k-call backfill is out of scope for
    // this change, and a test must never be the thing that spends it.

    fn current_abstract(subject: &str) -> String {
        format!("{MEMORY_L0_APPLICABILITY_PREFIX} {subject} misbehaves. Run `djinn doctor`.")
    }

    async fn seed_note(
        repo: &NoteRepository,
        project_id: &str,
        title: &str,
        note_type: &str,
    ) -> djinn_memory::Note {
        repo.create(project_id, title, "body", note_type, "[]")
            .await
            .unwrap()
    }

    fn drain(rx: &mut mpsc::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(id) = rx.try_recv() {
            out.push(id);
        }
        out
    }

    async fn sweep_fixture() -> (Database, String, NoteRepository) {
        let db = Database::open_in_memory().unwrap();
        // `make_project` ignores the path argument; nothing here touches disk.
        let project = make_project(&db, std::path::Path::new(".")).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        (db, project.id, repo)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_selects_only_notes_without_a_current_vintage_abstract() {
        let (db, project_id, repo) = sweep_fixture().await;

        let missing = seed_note(&repo, &project_id, "missing", "pitfall").await;
        let blank = seed_note(&repo, &project_id, "blank", "pitfall").await;
        let legacy = seed_note(&repo, &project_id, "legacy", "pitfall").await;
        let current = seed_note(&repo, &project_id, "current", "pitfall").await;

        repo.update_summaries(&blank.id, Some("   "), Some("ov"))
            .await
            .unwrap();
        repo.update_summaries(
            &legacy.id,
            Some("A terse pre-u46i one-liner about the thing."),
            Some("ov"),
        )
        .await
        .unwrap();
        repo.update_summaries(&current.id, Some(&current_abstract("dispatch")), Some("ov"))
            .await
            .unwrap();

        let mut selected = select_stale_abstract_note_ids(&db, None, 100, &[])
            .await
            .unwrap();
        selected.sort();

        let mut expected = vec![missing.id, blank.id, legacy.id];
        expected.sort();

        assert_eq!(selected, expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_filters_by_note_type() {
        let (db, project_id, repo) = sweep_fixture().await;

        let pitfall = seed_note(&repo, &project_id, "pitfall-note", "pitfall").await;
        seed_note(&repo, &project_id, "reference-note", "reference").await;

        let selected =
            select_stale_abstract_note_ids(&db, None, 100, &["pitfall".to_string()]).await;

        assert_eq!(selected.unwrap(), vec![pitfall.id]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_batches_and_enqueues_every_stale_note_exactly_once() {
        let (db, project_id, repo) = sweep_fixture().await;

        let mut expected = Vec::new();
        for index in 0..7 {
            let note = seed_note(&repo, &project_id, &format!("note-{index}"), "pitfall").await;
            expected.push(note.id);
        }
        expected.sort();

        let (tx, mut rx) = mpsc::channel(64);
        let report = run_abstract_sweep(
            &db,
            &tx,
            &AbstractSweepConfig {
                batch_size: 2,
                ..AbstractSweepConfig::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(report.selected, 7);
        assert_eq!(report.enqueued, 7);
        // 4 full/partial pages of work plus the terminating empty page.
        assert_eq!(report.batches, 4);
        assert!(!report.reached_cap);
        assert!(!report.sink_closed);

        let mut enqueued = drain(&mut rx);
        assert_eq!(enqueued.len(), 7, "no note enqueued twice or dropped");
        enqueued.sort();
        assert_eq!(enqueued, expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_is_resumable_and_never_redoes_completed_notes() {
        let (db, project_id, repo) = sweep_fixture().await;

        let mut ids = Vec::new();
        for index in 0..6 {
            ids.push(
                seed_note(&repo, &project_id, &format!("note-{index}"), "pitfall")
                    .await
                    .id,
            );
        }

        // First pass stops after 2 notes, as if the process were restarted.
        let (tx, mut rx) = mpsc::channel(64);
        let first = run_abstract_sweep(
            &db,
            &tx,
            &AbstractSweepConfig {
                batch_size: 10,
                max_notes: Some(2),
                note_types: Vec::new(),
            },
        )
        .await
        .unwrap();

        assert!(first.reached_cap);
        assert_eq!(first.enqueued, 2);
        let done = drain(&mut rx);
        assert_eq!(done.len(), 2);

        // The worker regenerated those two; they now carry current-vintage text.
        for id in &done {
            repo.update_summaries(id, Some(&current_abstract("the queue")), Some("ov"))
                .await
                .unwrap();
        }

        // A restarted sweep starts from a fresh cursor and must still skip them.
        let second = run_abstract_sweep(&db, &tx, &AbstractSweepConfig::default())
            .await
            .unwrap();
        let remaining = drain(&mut rx);

        assert_eq!(second.enqueued, 4);
        assert_eq!(remaining.len(), 4);
        for id in &done {
            assert!(
                !remaining.contains(id),
                "a regenerated note must not be swept again"
            );
        }

        let mut all: Vec<String> = done.into_iter().chain(remaining).collect();
        all.sort();
        ids.sort();
        assert_eq!(all, ids, "the two passes together cover the corpus once");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_cursor_advances_past_notes_the_worker_could_not_fix() {
        let (db, project_id, repo) = sweep_fixture().await;

        for index in 0..3 {
            seed_note(&repo, &project_id, &format!("note-{index}"), "pitfall").await;
        }

        // Nothing regenerates anything here — the provider is never called. The
        // run must still terminate, which is exactly what the keyset cursor buys.
        let (tx, mut rx) = mpsc::channel(64);
        let report = run_abstract_sweep(
            &db,
            &tx,
            &AbstractSweepConfig {
                batch_size: 1,
                ..AbstractSweepConfig::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(report.enqueued, 3);
        assert_eq!(drain(&mut rx).len(), 3);
        assert!(report.cursor.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_stops_when_the_worker_is_gone() {
        let (db, project_id, repo) = sweep_fixture().await;
        seed_note(&repo, &project_id, "note", "pitfall").await;

        let (tx, rx) = mpsc::channel(4);
        drop(rx);

        let report = run_abstract_sweep(&db, &tx, &AbstractSweepConfig::default())
            .await
            .unwrap();

        assert!(report.sink_closed);
        assert_eq!(report.enqueued, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn coverage_counter_separates_vintages() {
        let (db, project_id, repo) = sweep_fixture().await;

        let legacy = seed_note(&repo, &project_id, "legacy", "pitfall").await;
        let current = seed_note(&repo, &project_id, "current", "pitfall").await;
        seed_note(&repo, &project_id, "missing", "pitfall").await;

        repo.update_summaries(&legacy.id, Some("Old style one-liner."), Some("ov"))
            .await
            .unwrap();
        repo.update_summaries(
            &current.id,
            Some(&current_abstract("the sweep")),
            Some("ov"),
        )
        .await
        .unwrap();

        let coverage = abstract_vintage_coverage(&db, &["pitfall".to_string()])
            .await
            .unwrap();

        assert_eq!(coverage.total, 3);
        assert_eq!(coverage.with_abstract, 2);
        assert_eq!(coverage.current_vintage, 1);
        assert_eq!(coverage.legacy_vintage(), 1);
        assert_eq!(coverage.missing(), 1);
        assert!(!coverage.is_converged());
        assert!((coverage.current_ratio() - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn sweep_is_off_unless_explicitly_enabled() {
        assert_eq!(sweep_config_from_env(|_| None), None);
        assert_eq!(
            sweep_config_from_env(|key| (key == SWEEP_ENABLE_ENV).then(|| "0".to_string())),
            None
        );
        assert_eq!(
            sweep_config_from_env(|key| (key == SWEEP_ENABLE_ENV).then(|| "maybe".to_string())),
            None
        );
    }

    #[test]
    fn sweep_config_reads_bounds_from_env() {
        let config = sweep_config_from_env(|key| match key {
            SWEEP_ENABLE_ENV => Some("true".to_string()),
            SWEEP_BATCH_SIZE_ENV => Some("50".to_string()),
            SWEEP_MAX_NOTES_ENV => Some("500".to_string()),
            SWEEP_TYPES_ENV => Some("pitfall, case ,pattern,".to_string()),
            _ => None,
        })
        .expect("sweep should be enabled");

        assert_eq!(config.batch_size, 50);
        assert_eq!(config.max_notes, Some(500));
        assert_eq!(
            config.note_types,
            vec![
                "pitfall".to_string(),
                "case".to_string(),
                "pattern".to_string()
            ]
        );
    }

    #[test]
    fn sweep_config_rejects_nonsense_bounds() {
        let config = sweep_config_from_env(|key| match key {
            SWEEP_ENABLE_ENV => Some("on".to_string()),
            SWEEP_BATCH_SIZE_ENV => Some("0".to_string()),
            SWEEP_MAX_NOTES_ENV => Some("not-a-number".to_string()),
            _ => None,
        })
        .expect("sweep should be enabled");

        assert_eq!(config.batch_size, SWEEP_DEFAULT_BATCH_SIZE);
        assert_eq!(config.max_notes, None);
        assert!(config.note_types.is_empty());
    }

    #[test]
    fn vintage_pattern_tracks_the_prompt() {
        assert!(MEMORY_L0_ABSTRACT.contains(MEMORY_L0_APPLICABILITY_PREFIX));
        assert_eq!(current_vintage_pattern(), "Applies when%");
    }
}
