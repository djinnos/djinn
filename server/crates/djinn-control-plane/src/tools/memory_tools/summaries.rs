// MCP-owned note summary helpers and worker implementation.

use std::time::Duration;

use djinn_core::events::EventBus;
use djinn_db::repositories::note::{
    AbstractVintageCoverage, DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT, StaleAbstractNote,
    abstract_vintage_coverage, exhausted_abstract_regeneration_notes,
    record_abstract_regeneration_attempt, record_abstract_regeneration_success,
    select_stale_abstract_note_ids,
};
use djinn_db::{Database, Error as DbError, NoteRepository};
use djinn_provider::{
    CompletionRequest, complete, prompts::MEMORY_L0_ABSTRACT, prompts::MEMORY_L1_OVERVIEW,
    prompts::memory_l0_prompt_version, provider::LlmProvider, resolve_memory_provider_for_user,
};
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

        // Whether the L0 abstract came from the prompt or from the non-LLM
        // fallback decides whether this note counts as regenerated. A truncation
        // fallback is not a prompt-vintage abstract, so it must not stamp the
        // version — otherwise a provider outage would mark the whole corpus
        // "current" and convergence would be a lie.
        let generated_abstract =
            complete_summary(provider, SUMMARY_SYSTEM, &abstract_prompt, LLM_MAX_TOKENS).await;
        let regenerated = generated_abstract.is_some();
        let abstract_summary = generated_abstract
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
            return;
        }

        if regenerated
            && let Err(error) = record_abstract_regeneration_success(
                &self.db,
                &note.project_id,
                &note.id,
                memory_l0_prompt_version(),
            )
            .await
        {
            // The abstract is persisted; only the vintage stamp failed. The
            // note stays selectable, so the worst case is one repeat attempt
            // rather than a note wrongly reported as current.
            warn!(
                note_id = %note_id,
                error = %error,
                "note summary generation: failed to record regeneration vintage"
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

// ── Abstract vintage: the sweep ───────────────────────────────────────────────
//
// Staleness is decided in `djinn-db`
// (`repositories::note::abstract_regeneration`) against the L0 prompt's content
// hash, not by matching the abstract's prose against a literal prefix. The
// earlier text-matching predicate made convergence depend on the model emitting
// exact words: a note phrased differently was re-selected from a fresh cursor on
// every run and re-paid-for forever, and `is_converged()` was unreachable.
//
// This module owns orchestration only — no SQL. `check-raw-sql-boundary.sh`
// enforces that every direct sqlx call lives in `djinn-db`.

pub(crate) const SWEEP_DEFAULT_BATCH_SIZE: i64 = 200;

/// How many parked permalinks to name in the diagnostic log line.
const EXHAUSTED_DIAGNOSTIC_LIMIT: i64 = 20;

/// Default ceiling on notes enqueued per run.
///
/// A bound, not `None`. An unbounded default means the first operator to flip
/// the switch spends on the entire ~10.7k-note corpus in one go; 500 is a
/// deliberate first bite that surfaces quality and cost before committing.
/// Raise it with [`SWEEP_MAX_NOTES_ENV`] once a sample looks right.
pub(crate) const SWEEP_DEFAULT_MAX_NOTES: usize = 500;

/// Environment switches for the startup sweep. Absent/unset means **off**.
pub(crate) const SWEEP_ENABLE_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP";
pub(crate) const SWEEP_BATCH_SIZE_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP_BATCH_SIZE";
pub(crate) const SWEEP_MAX_NOTES_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP_MAX_NOTES";
pub(crate) const SWEEP_TYPES_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP_TYPES";
pub(crate) const SWEEP_ATTEMPT_LIMIT_ENV: &str = "DJINN_MEMORY_ABSTRACT_SWEEP_ATTEMPT_LIMIT";

/// Sentinel accepted by [`SWEEP_MAX_NOTES_ENV`] for "the whole remaining
/// corpus". Spelled out so an unbounded run is always a deliberate choice.
pub(crate) const SWEEP_MAX_NOTES_UNBOUNDED: &str = "all";

/// Bounds for one sweep run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbstractSweepConfig {
    /// Rows read per keyset page.
    pub batch_size: i64,
    /// Cap on notes enqueued in a single run. `None` means the whole remaining
    /// corpus and is only reachable by setting the env var to
    /// [`SWEEP_MAX_NOTES_UNBOUNDED`].
    pub max_notes: Option<usize>,
    /// Restrict to these `note_type` values. Empty means every type.
    pub note_types: Vec<String>,
    /// Enqueues a note may accumulate before it is parked and reported.
    pub attempt_limit: i32,
}

impl Default for AbstractSweepConfig {
    fn default() -> Self {
        Self {
            batch_size: SWEEP_DEFAULT_BATCH_SIZE,
            max_notes: Some(SWEEP_DEFAULT_MAX_NOTES),
            note_types: Vec::new(),
            attempt_limit: DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT,
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
    /// Last note id considered.
    pub cursor: Option<String>,
    /// Run stopped because `max_notes` was hit, not because work ran out.
    pub reached_cap: bool,
    /// Run stopped because the worker's receiver was gone.
    pub sink_closed: bool,
}

impl AbstractSweepReport {
    /// True when the run drained the queue rather than hitting a limit — the
    /// signal that a follow-up run would be a no-op.
    pub fn drained(&self) -> bool {
        !self.reached_cap && !self.sink_closed
    }
}

/// Enumerate notes awaiting regeneration and feed them to the summary worker.
///
/// Owns no generation logic: it pushes ids into the same
/// [`spawn_summary_backfill_worker`] channel the read-triggered path uses, so
/// exactly one place talks to a provider. `send().await` on a bounded channel is
/// also the rate limiter — the sweep cannot outrun the worker's
/// `SUMMARY_BACKFILL_DELAY` and so cannot stampede the provider.
///
/// Termination and convergence:
///
/// * every enqueue records an attempt, so a note that never succeeds is parked
///   after `attempt_limit` runs instead of being re-selected forever;
/// * a successful regeneration stamps the prompt version, so the note leaves
///   the stale set permanently and a repeat run is a no-op;
/// * the keyset cursor terminates a single pass even when nothing succeeds,
///   because attempts are recorded as we go.
pub async fn run_abstract_sweep(
    db: &Database,
    sink: &mpsc::Sender<String>,
    config: &AbstractSweepConfig,
) -> Result<AbstractSweepReport, DbError> {
    let prompt_version = memory_l0_prompt_version();
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

        let page = select_stale_abstract_note_ids(
            db,
            prompt_version,
            cursor.as_deref(),
            limit,
            &config.note_types,
            config.attempt_limit,
        )
        .await?;
        if page.is_empty() {
            break;
        }

        report.batches += 1;
        report.selected += page.len();

        for stale in page {
            let StaleAbstractNote {
                note_id,
                project_id,
            } = stale;
            cursor = Some(note_id.clone());
            report.cursor = cursor.clone();

            // Record the attempt BEFORE handing the note over. If the process
            // dies between here and generation the note has still consumed an
            // attempt, which is what keeps a crash loop bounded.
            record_abstract_regeneration_attempt(db, &project_id, &note_id, prompt_version).await?;

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

    // Unset means the bounded default, NOT the whole corpus. Removing the cap
    // requires spelling `all`, so an unbounded run is always deliberate.
    let max_notes = match lookup(SWEEP_MAX_NOTES_ENV) {
        None => Some(SWEEP_DEFAULT_MAX_NOTES),
        Some(raw) if raw.trim().eq_ignore_ascii_case(SWEEP_MAX_NOTES_UNBOUNDED) => None,
        Some(raw) => Some(
            raw.trim()
                .parse::<usize>()
                .ok()
                .filter(|value| *value > 0)
                .unwrap_or(SWEEP_DEFAULT_MAX_NOTES),
        ),
    };

    let note_types = lookup(SWEEP_TYPES_ENV)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let attempt_limit = lookup(SWEEP_ATTEMPT_LIMIT_ENV)
        .and_then(|raw| raw.trim().parse::<i32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT);

    Some(AbstractSweepConfig {
        batch_size,
        max_notes,
        note_types,
        attempt_limit,
    })
}

fn log_abstract_coverage(coverage: AbstractVintageCoverage, phase: &'static str) {
    info!(
        phase,
        total = coverage.total,
        current_vintage = coverage.current_vintage,
        // Work a sweep would still do. Zero means converged.
        pending = coverage.pending,
        // Parked at the attempt bound: these need a human, not another run.
        exhausted = coverage.exhausted,
        current_ratio = coverage.current_ratio(),
        converged = coverage.is_converged(),
        converged_with_failures = coverage.converged_with_failures(),
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

        let attempt_limit = sweep_config
            .as_ref()
            .map(|config| config.attempt_limit)
            .unwrap_or(DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT);

        match abstract_vintage_coverage(
            &observer_db,
            memory_l0_prompt_version(),
            &note_types,
            attempt_limit,
        )
        .await
        {
            Ok(coverage) => {
                log_abstract_coverage(coverage, "startup");
                // Parked notes need a human, not another run. Name them, or
                // "exhausted: 7" is a number nobody can act on.
                if coverage.exhausted > 0 {
                    match exhausted_abstract_regeneration_notes(
                        &observer_db,
                        memory_l0_prompt_version(),
                        &note_types,
                        attempt_limit,
                        EXHAUSTED_DIAGNOSTIC_LIMIT,
                    )
                    .await
                    {
                        Ok(permalinks) => warn!(
                            exhausted = coverage.exhausted,
                            attempt_limit,
                            sample = ?permalinks,
                            "memory L0 abstract: notes parked at the regeneration attempt limit; \
                             they will not be retried and need investigation"
                        ),
                        Err(error) => {
                            warn!(error = %error, "abstract vintage: parked-note lookup failed");
                        }
                    }
                }
            }
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
                    // True when the run drained the queue rather than hitting a
                    // limit: the signal that a follow-up run would be a no-op.
                    drained = report.drained(),
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
    use djinn_provider::prompts::MEMORY_L0_APPLICABILITY_PREFIX;
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

    fn compliant_abstract(subject: &str) -> String {
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

    /// Ids the sweep would enqueue right now, under the live prompt version.
    async fn stale_now(db: &Database, note_types: &[String]) -> Vec<String> {
        let mut ids: Vec<String> = select_stale_abstract_note_ids(
            db,
            memory_l0_prompt_version(),
            None,
            100,
            note_types,
            DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|note| note.note_id)
        .collect();
        ids.sort();
        ids
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_selects_by_recorded_vintage_not_by_abstract_prose() {
        let (db, project_id, repo) = sweep_fixture().await;

        let missing = seed_note(&repo, &project_id, "missing", "pitfall").await;
        let blank = seed_note(&repo, &project_id, "blank", "pitfall").await;
        let compliant_prose = seed_note(&repo, &project_id, "compliant-prose", "pitfall").await;
        let regenerated = seed_note(&repo, &project_id, "regenerated", "pitfall").await;

        repo.update_summaries(&blank.id, Some("   "), Some("ov"))
            .await
            .unwrap();
        // Opens with the prompt's lead-in but was never regenerated. Under the
        // old text-matching predicate this note looked current; it is stale.
        repo.update_summaries(
            &compliant_prose.id,
            Some(&compliant_abstract("dispatch")),
            Some("ov"),
        )
        .await
        .unwrap();
        // Regenerated, but the model phrased it its own way. Under the old
        // predicate this note was re-selected and re-paid-for on every run.
        repo.update_summaries(
            &regenerated.id,
            Some("Reach for this when the queue wedges."),
            Some("ov"),
        )
        .await
        .unwrap();
        record_abstract_regeneration_success(
            &db,
            &project_id,
            &regenerated.id,
            memory_l0_prompt_version(),
        )
        .await
        .unwrap();

        let mut expected = vec![missing.id, blank.id, compliant_prose.id];
        expected.sort();

        assert_eq!(
            stale_now(&db, &[]).await,
            expected,
            "staleness follows the recorded prompt version, never the prose"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_filters_by_note_type() {
        let (db, project_id, repo) = sweep_fixture().await;

        let pitfall = seed_note(&repo, &project_id, "pitfall-note", "pitfall").await;
        seed_note(&repo, &project_id, "reference-note", "reference").await;

        assert_eq!(
            stale_now(&db, &["pitfall".to_string()]).await,
            vec![pitfall.id]
        );
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
                ..AbstractSweepConfig::default()
            },
        )
        .await
        .unwrap();

        assert!(first.reached_cap);
        assert_eq!(first.enqueued, 2);
        let done = drain(&mut rx);
        assert_eq!(done.len(), 2);

        // The worker regenerated those two: their abstracts are rewritten and
        // the current prompt version is recorded against them.
        for id in &done {
            repo.update_summaries(id, Some(&compliant_abstract("the queue")), Some("ov"))
                .await
                .unwrap();
            record_abstract_regeneration_success(&db, &project_id, id, memory_l0_prompt_version())
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
            Some(&compliant_abstract("the sweep")),
            Some("ov"),
        )
        .await
        .unwrap();
        record_abstract_regeneration_success(
            &db,
            &project_id,
            &current.id,
            memory_l0_prompt_version(),
        )
        .await
        .unwrap();

        let coverage = abstract_vintage_coverage(
            &db,
            memory_l0_prompt_version(),
            &["pitfall".to_string()],
            DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT,
        )
        .await
        .unwrap();

        assert_eq!(coverage.total, 3);
        assert_eq!(coverage.current_vintage, 1);
        assert_eq!(coverage.pending, 2);
        assert_eq!(coverage.exhausted, 0);
        assert!(!coverage.is_converged());
        assert!((coverage.current_ratio() - 1.0 / 3.0).abs() < 1e-9);
    }

    /// Property 4: `is_converged()` is reachable even when a note can never be
    /// regenerated.
    ///
    /// This is the defect the whole change exists to fix. The first cut defined
    /// convergence as "every note current", so one permanently non-compliant
    /// note kept `is_converged()` structurally false and the sweep re-paid for
    /// that note on every run. Here the sweep is driven for real, the "worker"
    /// only ever succeeds on one of the two notes, and convergence still lands.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn is_converged_is_reachable_with_a_permanently_failing_note() {
        let (db, project_id, repo) = sweep_fixture().await;

        let ok = seed_note(&repo, &project_id, "regenerates-fine", "pitfall").await;
        let doomed = seed_note(&repo, &project_id, "never-regenerates", "pitfall").await;

        let config = AbstractSweepConfig {
            batch_size: 10,
            note_types: vec!["pitfall".to_string()],
            ..AbstractSweepConfig::default()
        };

        for _ in 0..config.attempt_limit {
            let (tx, mut rx) = mpsc::channel(64);
            run_abstract_sweep(&db, &tx, &config).await.unwrap();
            // The worker succeeds on `ok` and fails on `doomed` every time.
            for id in drain(&mut rx) {
                if id == ok.id {
                    record_abstract_regeneration_success(
                        &db,
                        &project_id,
                        &id,
                        memory_l0_prompt_version(),
                    )
                    .await
                    .unwrap();
                }
            }
        }

        let coverage = abstract_vintage_coverage(
            &db,
            memory_l0_prompt_version(),
            &config.note_types,
            config.attempt_limit,
        )
        .await
        .unwrap();

        assert_eq!(coverage.total, 2);
        assert_eq!(coverage.current_vintage, 1);
        assert_eq!(coverage.pending, 0, "no work is left to do");
        assert_eq!(coverage.exhausted, 1, "the doomed note is parked, not lost");
        assert!(
            coverage.is_converged(),
            "convergence must be reachable with a permanently failing note"
        );
        assert!(
            coverage.converged_with_failures(),
            "and it must report that it converged WITH failures"
        );

        // The parked note is named, so an operator can act on it.
        let parked = exhausted_abstract_regeneration_notes(
            &db,
            memory_l0_prompt_version(),
            &config.note_types,
            config.attempt_limit,
            100,
        )
        .await
        .unwrap();
        assert_eq!(parked, vec![doomed.permalink]);
    }

    /// Property 5: re-running a completed sweep enqueues nothing.
    ///
    /// Asserted by actually running `run_abstract_sweep` twice against a fresh
    /// sink, not by re-evaluating a predicate: the no-op is a property of the
    /// orchestration, and the first cut's predicate was exactly what lied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rerunning_a_completed_sweep_is_a_no_op() {
        let (db, project_id, repo) = sweep_fixture().await;

        for index in 0..5 {
            seed_note(&repo, &project_id, &format!("note-{index}"), "pitfall").await;
        }

        let (tx, mut rx) = mpsc::channel(64);
        let first = run_abstract_sweep(&db, &tx, &AbstractSweepConfig::default())
            .await
            .unwrap();
        assert_eq!(first.enqueued, 5);
        assert!(first.drained());

        // The worker regenerated everything it was handed.
        for id in drain(&mut rx) {
            record_abstract_regeneration_success(&db, &project_id, &id, memory_l0_prompt_version())
                .await
                .unwrap();
        }

        let (tx2, mut rx2) = mpsc::channel(64);
        let second = run_abstract_sweep(&db, &tx2, &AbstractSweepConfig::default())
            .await
            .unwrap();

        assert_eq!(second.enqueued, 0, "a completed sweep must cost nothing");
        assert_eq!(second.selected, 0);
        assert_eq!(second.batches, 0);
        assert!(second.drained());
        assert!(drain(&mut rx2).is_empty());
    }

    /// Property 8: the attempt is recorded at ENQUEUE time.
    ///
    /// A note handed to a sink that is then gone has still consumed an attempt,
    /// which is what bounds a crash loop: without it, a process dying between
    /// enqueue and generation would restart the same note forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attempts_are_recorded_at_enqueue_so_a_dead_sink_still_consumes_budget() {
        let (db, project_id, repo) = sweep_fixture().await;
        let note = seed_note(&repo, &project_id, "note", "pitfall").await;

        let config = AbstractSweepConfig::default();
        for round in 1..=config.attempt_limit {
            let (tx, rx) = mpsc::channel(4);
            drop(rx);

            let report = run_abstract_sweep(&db, &tx, &config).await.unwrap();
            assert!(report.sink_closed);
            assert_eq!(report.enqueued, 0, "nothing was ever delivered");

            let coverage = abstract_vintage_coverage(
                &db,
                memory_l0_prompt_version(),
                &config.note_types,
                config.attempt_limit,
            )
            .await
            .unwrap();
            let exhausted = round >= config.attempt_limit;
            assert_eq!(
                coverage.exhausted,
                i64::from(exhausted),
                "after {round} of {} enqueues",
                config.attempt_limit
            );
        }

        // The budget was consumed by enqueues alone, so the note is now parked
        // and no further run will pay for it.
        assert!(stale_now(&db, &[]).await.is_empty());
        assert_eq!(
            exhausted_abstract_regeneration_notes(
                &db,
                memory_l0_prompt_version(),
                &[],
                config.attempt_limit,
                100,
            )
            .await
            .unwrap(),
            vec![note.permalink]
        );
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
        assert_eq!(
            config.attempt_limit, DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT,
            "an unset attempt limit keeps the bound, it does not remove it"
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
        assert_eq!(config.max_notes, Some(SWEEP_DEFAULT_MAX_NOTES));
        assert!(config.note_types.is_empty());
    }

    /// Property 10: an unset cap is the bounded default, not "the whole corpus".
    ///
    /// The foot-gun: unset used to mean unbounded, so enabling the sweep at all
    /// committed to ~10.7k live LLM calls in one run.
    #[test]
    fn unset_max_notes_falls_back_to_the_bounded_default_not_unbounded() {
        let config =
            sweep_config_from_env(|key| (key == SWEEP_ENABLE_ENV).then(|| "1".to_string()))
                .expect("sweep should be enabled");

        assert_eq!(config.max_notes, Some(SWEEP_DEFAULT_MAX_NOTES));
        assert_ne!(config.max_notes, None, "unset must never mean unbounded");
    }

    /// Property 11: unbounded is reachable only by spelling the sentinel.
    #[test]
    fn unbounded_max_notes_requires_the_explicit_all_sentinel() {
        for spelling in ["all", "ALL", "All", "  all  "] {
            let config = sweep_config_from_env(|key| match key {
                SWEEP_ENABLE_ENV => Some("1".to_string()),
                SWEEP_MAX_NOTES_ENV => Some(spelling.to_string()),
                _ => None,
            })
            .expect("sweep should be enabled");

            assert_eq!(config.max_notes, None, "{spelling:?} must mean unbounded");
        }

        assert_eq!(SWEEP_MAX_NOTES_UNBOUNDED, "all");
    }

    /// Property 12: a malformed or zero cap degrades to the bounded default and
    /// never to unbounded — a typo must not become unlimited spend.
    #[test]
    fn invalid_max_notes_falls_back_to_the_bounded_default_never_to_unbounded() {
        for raw in ["not-a-number", "0", "-5", "", "   ", "all-of-them", "1.5"] {
            let config = sweep_config_from_env(|key| match key {
                SWEEP_ENABLE_ENV => Some("1".to_string()),
                SWEEP_MAX_NOTES_ENV => Some(raw.to_string()),
                _ => None,
            })
            .expect("sweep should be enabled");

            assert_eq!(
                config.max_notes,
                Some(SWEEP_DEFAULT_MAX_NOTES),
                "{raw:?} must degrade to the bounded default"
            );
        }
    }

    /// Property 13: the attempt limit is configurable, and nonsense keeps the
    /// default bound rather than disabling it.
    #[test]
    fn attempt_limit_parses_and_invalid_values_keep_the_default_bound() {
        let config = sweep_config_from_env(|key| match key {
            SWEEP_ENABLE_ENV => Some("1".to_string()),
            SWEEP_ATTEMPT_LIMIT_ENV => Some(" 7 ".to_string()),
            _ => None,
        })
        .expect("sweep should be enabled");
        assert_eq!(config.attempt_limit, 7);

        for raw in ["0", "-1", "not-a-number", "", "  "] {
            let config = sweep_config_from_env(|key| match key {
                SWEEP_ENABLE_ENV => Some("1".to_string()),
                SWEEP_ATTEMPT_LIMIT_ENV => Some(raw.to_string()),
                _ => None,
            })
            .expect("sweep should be enabled");

            assert_eq!(
                config.attempt_limit, DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT,
                "{raw:?} must keep the default bound, never remove it"
            );
        }
    }

    #[test]
    fn the_applicability_prefix_is_a_prose_convention_not_the_vintage_marker() {
        // The prompt still asks for the readability lead-in …
        assert!(MEMORY_L0_ABSTRACT.contains(MEMORY_L0_APPLICABILITY_PREFIX));

        // … but the marker the sweep compares against is the prompt's content
        // hash, which no abstract's wording can imitate. Its stability matters:
        // it is what makes a completed sweep stay completed.
        let version = memory_l0_prompt_version();
        assert!(version.starts_with("l0-"), "unexpected shape: {version}");
        assert_eq!(version, memory_l0_prompt_version());
        assert!(!version.contains(MEMORY_L0_APPLICABILITY_PREFIX));
    }
}
