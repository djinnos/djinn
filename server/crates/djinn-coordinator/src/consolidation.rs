use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use djinn_core::models::{DjinnSettings, KnowledgeInjectionConfig};
use djinn_db::{
    BoundedCluster, CONSOLIDATION_DEFAULT_SCORE_THRESHOLD, CommitConsolidationCanonical,
    ConsolidationCommitOutcome, ConsolidationPartitionKey, CreateCanonicalConsolidatedNote,
    CreateConsolidationRunMetric, Database, DbNoteGroup, NoteConsolidationRepository,
    NoteRevisionReason, PartitionPressureMetric, SettingsRepository,
};
use djinn_memory::ConsolidationCluster;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const CONSOLIDATION_MIN_CLUSTER_SIZE: usize = 3;
const CONSOLIDATION_TAGS: &str = r#"["canonical","consolidated"]"#;

pub(super) trait ConsolidationRunner: Send + Sync {
    /// Run consolidation for a note group across all notes (unscoped).
    /// Retained for backward compatibility and direct invocations outside the
    /// periodic session-scoped consolidation loop.
    #[allow(dead_code)]
    fn run_for_group<'a>(
        &'a self,
        group: DbNoteGroup,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>>;

    /// Run consolidation for a note group scoped to a single session.
    /// Only notes linked to `session_id` via `consolidated_note_provenance`
    /// are considered as duplicate candidates.
    fn run_for_group_in_session<'a>(
        &'a self,
        group: DbNoteGroup,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>>;
}

pub(super) struct DbConsolidationRunner {
    db: Database,
}

impl DbConsolidationRunner {
    pub(super) fn new(db: Database) -> Self {
        Self { db }
    }
}

impl ConsolidationRunner for DbConsolidationRunner {
    fn run_for_group<'a>(
        &'a self,
        group: DbNoteGroup,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let repo = NoteConsolidationRepository::new(self.db.clone());
            let started_at = now_rfc3339();
            let clusters = repo
                .likely_duplicate_clusters(&group.project_id, &group.note_type)
                .await?;
            consolidate_clusters(&repo, &group, &clusters, &started_at).await
        })
    }

    fn run_for_group_in_session<'a>(
        &'a self,
        group: DbNoteGroup,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let repo = NoteConsolidationRepository::new(self.db.clone());
            let started_at = now_rfc3339();
            let clusters = repo
                .likely_duplicate_clusters_for_session(
                    &group.project_id,
                    &group.note_type,
                    &session_id,
                )
                .await?;
            consolidate_clusters(&repo, &group, &clusters, &started_at).await
        })
    }
}

/// Shared consolidation logic: filter qualifying clusters, create canonical
/// notes, and record run metrics.
async fn consolidate_clusters(
    repo: &NoteConsolidationRepository,
    group: &DbNoteGroup,
    clusters: &[ConsolidationCluster],
    started_at: &str,
) -> djinn_db::Result<()> {
    let qualifying_clusters = clusters
        .iter()
        .filter(|cluster| cluster.note_ids.len() >= CONSOLIDATION_MIN_CLUSTER_SIZE)
        .collect::<Vec<_>>();

    if qualifying_clusters.is_empty() {
        return Ok(());
    }

    let mut consolidated_note_count = 0_i64;
    let mut source_note_count = 0_i64;
    let mut superseded_source_note_count = 0_i64;

    for cluster in qualifying_clusters.iter().copied() {
        let source_session_ids = repo
            .resolve_source_session_ids(&group.project_id, &cluster.note_ids)
            .await?;
        let source_session_refs = source_session_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let synthesized = synthesize_cluster(cluster);
        let reason = NoteRevisionReason::new("consolidation:create canonical cluster note")
            .map_err(|e| djinn_db::error::DbError::InvalidData(e.to_string()))?;

        let created = repo
            .create_canonical_consolidated_note(CreateCanonicalConsolidatedNote {
                project_id: &group.project_id,
                note_type: &group.note_type,
                title: &synthesized.title,
                content: &synthesized.content,
                tags: CONSOLIDATION_TAGS,
                abstract_: synthesized.abstract_.as_deref(),
                overview: synthesized.overview.as_deref(),
                confidence: synthesized.confidence,
                reason,
                source_session_ids: &source_session_refs,
                scope_paths: &synthesized.scope_paths,
                source_note_ids: &cluster.note_ids,
            })
            .await?;

        consolidated_note_count += 1;
        source_note_count += cluster.note_ids.len() as i64;
        superseded_source_note_count += created.superseded_source_note_count as i64;
    }

    let completed_at = now_rfc3339();
    repo.create_run_metric(CreateConsolidationRunMetric {
        project_id: &group.project_id,
        note_type: &group.note_type,
        status: "completed",
        scanned_note_count: group.note_count,
        candidate_cluster_count: clusters.len() as i64,
        consolidated_cluster_count: qualifying_clusters.len() as i64,
        consolidated_note_count,
        source_note_count,
        decayed_note_count: 0,
        archived_note_count: 0,
        superseded_source_note_count,
        admission_dropped_note_count: 0,
        started_at,
        completed_at: Some(&completed_at),
        error_message: None,
    })
    .await?;

    Ok(())
}

struct SynthesizedClusterNote {
    title: String,
    content: String,
    abstract_: Option<String>,
    overview: Option<String>,
    confidence: f64,
    scope_paths: String,
}

fn synthesize_cluster(cluster: &ConsolidationCluster) -> SynthesizedClusterNote {
    let mut notes = cluster.notes.iter().collect::<Vec<_>>();
    notes.sort_by(|left, right| {
        left.permalink
            .cmp(&right.permalink)
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut scope_union: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for note in &notes {
        match serde_json::from_str::<Vec<String>>(&note.scope_paths) {
            Ok(paths) => {
                for p in paths {
                    scope_union.insert(p);
                }
            }
            Err(error) => {
                tracing::warn!(
                    permalink = %note.permalink,
                    error = %error,
                    "synthesize_cluster: failed to parse source note scope_paths; treating as empty"
                );
            }
        }
    }
    let scope_paths = serde_json::to_string(&scope_union.into_iter().collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".to_string());

    let primary = notes[0];
    let title = format!("Canonical {}: {}", primary.note_type, primary.title.trim());
    let abstracts =
        collect_unique_fragments(notes.iter().filter_map(|note| note.abstract_.as_deref()));
    let overviews =
        collect_unique_fragments(notes.iter().filter_map(|note| note.overview.as_deref()));
    let note_titles = notes
        .iter()
        .map(|note| format!("- {} ({})", note.title.trim(), note.permalink))
        .collect::<Vec<_>>()
        .join("\n");
    let note_bodies = notes
        .iter()
        .map(|note| {
            let summary = preferred_summary(note);
            format!(
                "### {}\n{}\n\nSource permalink: {}",
                note.title.trim(),
                summary,
                note.permalink
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let abstract_ = abstracts.first().cloned().or_else(|| {
        overviews
            .first()
            .map(|overview| truncate_for_summary(overview, 280))
    });
    let overview = if abstracts.is_empty() && overviews.is_empty() {
        None
    } else {
        Some(
            abstracts
                .iter()
                .chain(overviews.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        )
    };

    let content = format!(
        "# {}\n\n## Consolidated summary\n{}\n\n## Source notes\n{}\n\n## Synthesized details\n{}",
        title,
        abstract_
            .clone()
            .or_else(|| overview.clone())
            .unwrap_or_else(|| truncate_for_summary(&preferred_summary(primary), 280)),
        note_titles,
        note_bodies
    );

    SynthesizedClusterNote {
        title,
        content,
        abstract_,
        overview,
        confidence: bounded_confidence(notes.len()),
        scope_paths,
    }
}

fn preferred_summary(note: &djinn_memory::ConsolidationNote) -> String {
    note.abstract_
        .as_deref()
        .or(note.overview.as_deref())
        .unwrap_or(note.content.as_str())
        .trim()
        .to_string()
}

fn collect_unique_fragments<'a>(fragments: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    fragments
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .filter(|fragment| seen.insert((*fragment).to_string()))
        .map(ToString::to_string)
        .collect()
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let truncated = trimmed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    format!("{}…", truncated.trim_end())
}

fn bounded_confidence(cluster_size: usize) -> f64 {
    (0.5 + 0.05 * cluster_size as f64).min(0.8)
}

fn now_rfc3339() -> String {
    match OffsetDateTime::now_utc().format(&Rfc3339) {
        Ok(timestamp) => timestamp,
        Err(error) => {
            tracing::warn!(%error, "failed to format consolidation timestamp as RFC3339");
            OffsetDateTime::now_utc().to_string()
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Bounded, gated consolidation run (proposal `t5rn`, T2 + T6)
// ═════════════════════════════════════════════════════════════════════════════

/// Environment variable names for the enablement gate.
///
/// The partition is read from configuration on purpose: nothing in this crate
/// may name a particular deployment's project, session, or note type.
pub const CONSOLIDATION_WRITES_ENV: &str = "DJINN_CONSOLIDATION_CANONICAL_WRITES";
pub const CONSOLIDATION_PROJECT_ENV: &str = "DJINN_CONSOLIDATION_PROJECT_ID";
pub const CONSOLIDATION_SESSION_ENV: &str = "DJINN_CONSOLIDATION_SESSION_ID";
pub const CONSOLIDATION_NOTE_TYPE_ENV: &str = "DJINN_CONSOLIDATION_NOTE_TYPE";
pub const CONSOLIDATION_THRESHOLD_ENV: &str = "DJINN_CONSOLIDATION_SCORE_THRESHOLD";

/// The exactly-one-partition enablement gate.
///
/// Canonical writes default **off**. An enabled request is valid only when
/// configuration supplies exactly one non-blank `project_id`, `session_id`, and
/// eligible `note_type`; missing, extra, wildcard, or multi-valued keys are
/// rejected before any synthesis.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationEnablement {
    pub canonical_writes_enabled: bool,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub note_type: Option<String>,
    pub score_threshold: f64,
}

impl Default for ConsolidationEnablement {
    fn default() -> Self {
        Self {
            canonical_writes_enabled: false,
            project_id: None,
            session_id: None,
            note_type: None,
            score_threshold: CONSOLIDATION_DEFAULT_SCORE_THRESHOLD,
        }
    }
}

impl ConsolidationEnablement {
    /// Read the gate from the process environment. Absent keys stay `None` and
    /// absent/unparseable enablement stays off.
    pub fn from_env() -> Self {
        fn non_blank(name: &str) -> Option<String> {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        }
        Self {
            canonical_writes_enabled: non_blank(CONSOLIDATION_WRITES_ENV).is_some_and(|value| {
                matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            }),
            project_id: non_blank(CONSOLIDATION_PROJECT_ENV),
            session_id: non_blank(CONSOLIDATION_SESSION_ENV),
            note_type: non_blank(CONSOLIDATION_NOTE_TYPE_ENV),
            // A configured value that is not a finite *positive* number falls
            // back to the default rather than arming an indiscriminate merge;
            // the gate rejects it again before any synthesis.
            score_threshold: non_blank(CONSOLIDATION_THRESHOLD_ENV)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or(CONSOLIDATION_DEFAULT_SCORE_THRESHOLD),
        }
    }

    /// Resolve the requested partition, or the reason it was rejected.
    fn resolve_partition(&self) -> std::result::Result<ConsolidationPartitionKey, String> {
        let (Some(project_id), Some(session_id), Some(note_type)) = (
            self.project_id.as_ref(),
            self.session_id.as_ref(),
            self.note_type.as_ref(),
        ) else {
            return Err(
                "consolidation requires exactly one project_id, session_id, and note_type"
                    .to_owned(),
            );
        };
        let key = ConsolidationPartitionKey {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            note_type: note_type.clone(),
        };
        key.validate().map_err(|error| error.to_string())?;
        // A non-positive threshold would admit every note to every other note,
        // committing an arbitrary 8-note merge on a subtractive path. Refuse it
        // rather than clamping, so the misconfiguration is visible.
        djinn_db::minimum_valid_score_threshold(self.score_threshold)
            .map_err(|error| error.to_string())?;
        Ok(key)
    }
}

/// Settings key holding the serialized `DjinnSettings` blob.
const SETTINGS_RAW_KEY: &str = "settings.raw";

/// Report per-`(project_id, note_type)` retrieval pressure once per housekeeping
/// sweep (proposal `t5rn`, T6).
///
/// This is strictly report-only. It performs one read of the configured
/// injection budget and one grouped snapshot query, emits the readings, and
/// stops. It creates no prompt policy, no task, no cooldown, no deletion, and no
/// automatic actuator, and it is never invoked on a write path.
///
/// `injectable_slots` is the configured `knowledge_injection_limit`. That budget
/// is shared across the eligible note types rather than split per type, so the
/// per-type ceiling is the whole limit: each type could in principle fill the
/// entire context build. Reporting it per type keeps the ratio comparable
/// across types without inventing a split the retrieval path does not implement.
pub async fn report_partition_pressure(db: &Database) -> Vec<PartitionPressureMetric> {
    let settings = match SettingsRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .get(SETTINGS_RAW_KEY)
        .await
    {
        Ok(setting) => setting
            .map(|setting| DjinnSettings::from_db_value(&setting.value))
            .unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%error, "consolidation pressure: failed to load settings; using defaults");
            DjinnSettings::default()
        }
    };
    let injectable_slots = match KnowledgeInjectionConfig::from_settings_and_env(&settings) {
        Ok(config) => i64::from(config.knowledge_injection_limit),
        Err(error) => {
            tracing::warn!(%error, "consolidation pressure: invalid knowledge injection config");
            i64::from(KnowledgeInjectionConfig::DEFAULT_KNOWLEDGE_INJECTION_LIMIT)
        }
    };
    let slots_by_note_type = djinn_db::CONSOLIDATION_ELIGIBLE_NOTE_TYPES
        .iter()
        .map(|note_type| ((*note_type).to_owned(), injectable_slots))
        .collect::<std::collections::HashMap<_, _>>();

    let metrics = match NoteConsolidationRepository::new(db.clone())
        .partition_pressure_metrics(&slots_by_note_type)
        .await
    {
        Ok(metrics) => metrics,
        Err(error) => {
            tracing::warn!(%error, "consolidation pressure: failed to snapshot partition pressure");
            return Vec::new();
        }
    };

    for metric in &metrics {
        tracing::info!(
            project_id = %metric.project_id,
            note_type = %metric.note_type,
            eligible_notes = metric.eligible_notes,
            injectable_slots = metric.injectable_slots,
            oversubscription_ratio = metric.oversubscription_ratio,
            unbounded_pressure = metric.unbounded_pressure,
            "consolidation partition pressure"
        );
    }
    metrics
}

/// The committed effects of the single canonical transaction a run may perform.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationWriteResult {
    pub canonical_note_id: String,
    pub consolidation_attempt_id: String,
    pub canonical_body_digest: String,
    pub canonical_provenance_session_ids: Vec<String>,
    pub supersedes_source_note_ids: Vec<String>,
    pub final_source_statuses: Vec<(String, String)>,
}

/// Machine-readable outcome of one bounded consolidation run.
///
/// There is exactly one requested partition, at most one `write_result`, and a
/// `deferred_clusters` entry for every other qualifying cluster. A disabled,
/// rejected, empty, or conflict run has no `write_result`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConsolidationRunReport {
    pub requested_partition: Option<ConsolidationPartitionKey>,
    pub canonical_writes_enabled: bool,
    pub rejection_reason: Option<String>,
    pub conflict_reason: Option<String>,
    pub input_count: usize,
    pub overflow_count: usize,
    pub admission_comparisons: usize,
    pub qualifying_cluster_count: usize,
    pub write_result: Option<ConsolidationWriteResult>,
    /// Sorted source IDs of every qualifying cluster this run did **not**
    /// synthesize or mutate.
    pub deferred_clusters: Vec<Vec<String>>,
}

/// Execute one bounded consolidation run for the configured partition.
///
/// Regardless of enablement the run reports candidates, overflow, and every
/// qualifying cluster. When enabled and valid it commits **at most the first**
/// deterministic qualifying cluster and defers all the rest; widening either
/// the partition or the one-canonical-per-run limit is out of scope.
pub async fn run_bounded_consolidation(
    db: &Database,
    config: &ConsolidationEnablement,
) -> djinn_db::Result<ConsolidationRunReport> {
    let mut report = ConsolidationRunReport {
        canonical_writes_enabled: config.canonical_writes_enabled,
        ..ConsolidationRunReport::default()
    };

    let partition = match config.resolve_partition() {
        Ok(partition) => partition,
        Err(reason) => {
            report.rejection_reason = Some(reason);
            return Ok(report);
        }
    };
    report.requested_partition = Some(partition.clone());

    let repo = NoteConsolidationRepository::new(db.clone());
    let outcome = repo
        .bounded_clusters_for_partition(&partition, config.score_threshold)
        .await?;
    report.input_count = outcome.input_count;
    report.overflow_count = outcome.overflow_count;
    report.admission_comparisons = outcome.admission_comparisons;
    report.qualifying_cluster_count = outcome.clusters.len();

    let mut clusters = outcome.clusters.into_iter();
    let Some(first) = clusters.next() else {
        return Ok(report);
    };
    let deferred = clusters.collect::<Vec<_>>();

    if !config.canonical_writes_enabled {
        // Disabled: report the first cluster as deferred too. Nothing is
        // synthesized and nothing is mutated.
        report.deferred_clusters = std::iter::once(first)
            .chain(deferred)
            .map(|cluster| cluster.source_note_ids)
            .collect();
        return Ok(report);
    }

    report.deferred_clusters = deferred
        .iter()
        .map(|cluster| cluster.source_note_ids.clone())
        .collect();

    let synthesized = synthesize_bounded_cluster(&first);
    let reason = NoteRevisionReason::new("consolidation:create canonical cluster note")
        .map_err(|e| djinn_db::error::DbError::InvalidData(e.to_string()))?;
    let commit = repo
        .commit_consolidation_canonical(CommitConsolidationCanonical {
            partition: &partition,
            source_note_ids: &first.source_note_ids,
            title: &synthesized.title,
            content: &synthesized.content,
            abstract_: synthesized.abstract_.as_deref(),
            overview: synthesized.overview.as_deref(),
            confidence: synthesized.confidence,
            scope_paths: &synthesized.scope_paths,
            reason,
        })
        .await?;

    match commit {
        ConsolidationCommitOutcome::Committed(committed)
        | ConsolidationCommitOutcome::AlreadyCommitted(committed) => {
            report.write_result = Some(ConsolidationWriteResult {
                canonical_note_id: committed.canonical_note_id,
                consolidation_attempt_id: committed.consolidation_attempt_id,
                canonical_body_digest: committed.canonical_body_digest,
                canonical_provenance_session_ids: committed.canonical_provenance_session_ids,
                supersedes_source_note_ids: committed.supersedes_source_note_ids,
                final_source_statuses: committed.final_source_statuses,
            });
        }
        ConsolidationCommitOutcome::Conflict(conflict) => {
            // A conflict creates nothing, so the attempted cluster joins the
            // deferred list and no `write_result` is emitted.
            report.conflict_reason = Some(format!("{:?}: {}", conflict.reason, conflict.detail));
            report.deferred_clusters.insert(0, first.source_note_ids);
        }
    }

    Ok(report)
}

fn synthesize_bounded_cluster(cluster: &BoundedCluster) -> SynthesizedClusterNote {
    synthesize_cluster(&ConsolidationCluster {
        note_ids: cluster.source_note_ids.clone(),
        notes: cluster.ordered_notes.clone(),
        edges: Vec::new(),
    })
}

pub(super) async fn run_note_consolidation(
    db: &Database,
    consolidation_runner: &Arc<dyn ConsolidationRunner>,
) {
    let repo = NoteConsolidationRepository::new(db.clone());

    // Session-scoped consolidation: discover sessions that have provenance
    // entries and consolidate per-session to avoid merging unrelated
    // cross-session notes (ADR-045 §5).
    let session_ids = match repo.list_sessions_with_provenance().await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "CoordinatorActor: failed to list sessions with provenance for consolidation"
            );
            return;
        }
    };

    for session_id in session_ids {
        let groups = match repo.list_db_note_groups_for_session(&session_id).await {
            Ok(groups) => groups,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "CoordinatorActor: failed to list session note groups for consolidation"
                );
                continue;
            }
        };

        for group in groups {
            if let Err(error) = consolidation_runner
                .run_for_group_in_session(group.clone(), session_id.clone())
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    project_id = %group.project_id,
                    note_type = %group.note_type,
                    error = %error,
                    "CoordinatorActor: failed to run session-scoped DB note consolidation"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Instant as StdInstant;

    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::roles::RoleRegistry;
    use crate::test_helpers;
    use crate::{
        AutoMergeTracker, BackgroundWorkTracker, DEFAULT_MODEL_ID, PrCleanupConfig, STUCK_INTERVAL,
        SharedCoordinatorState,
    };
    use djinn_db::{CreateSessionParams, NoteRepository, SessionRepository};
    use djinn_provider::catalog::CatalogService;
    use djinn_provider::catalog::health::HealthTracker;
    use djinn_provider::rate_limit::{activate_suppression_window, clear_suppression_window};
    use djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};

    use super::super::actor::CoordinatorActor;

    struct RecordingConsolidationRunner {
        calls: Arc<Mutex<Vec<djinn_db::DbNoteGroup>>>,
        session_calls: Arc<Mutex<Vec<(djinn_db::DbNoteGroup, String)>>>,
    }

    impl RecordingConsolidationRunner {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                session_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn session_groups(&self) -> Vec<(djinn_db::DbNoteGroup, String)> {
            self.session_calls.lock().unwrap().clone()
        }
    }

    impl ConsolidationRunner for RecordingConsolidationRunner {
        fn run_for_group<'a>(
            &'a self,
            group: djinn_db::DbNoteGroup,
        ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(group);
                Ok(())
            })
        }

        fn run_for_group_in_session<'a>(
            &'a self,
            group: djinn_db::DbNoteGroup,
            session_id: String,
        ) -> Pin<Box<dyn Future<Output = djinn_db::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.session_calls.lock().unwrap().push((group, session_id));
                Ok(())
            })
        }
    }

    fn test_actor(
        db: &Database,
        tx: &broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
        runner: Arc<dyn ConsolidationRunner>,
    ) -> CoordinatorActor {
        CoordinatorActor {
            receiver: tokio::sync::mpsc::channel(1).1,
            events: tx.subscribe(),
            cancel: CancellationToken::new(),
            tick: tokio::time::interval(STUCK_INTERVAL),
            db: db.clone(),
            coordinator_incarnation_id: uuid::Uuid::now_v7().to_string(),
            boot_at: ::time::OffsetDateTime::now_utc(),
            events_tx: tx.clone(),
            pool: SlotPoolHandle::spawn(
                test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
                CancellationToken::new(),
                SlotPoolConfig {
                    models: vec![ModelSlotConfig {
                        model_id: DEFAULT_MODEL_ID.to_owned(),
                        max_slots: 1,
                        roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
                    }],
                    role_priorities: std::collections::HashMap::new(),
                },
            ),
            catalog: CatalogService::new(),
            health: HealthTracker::new(),
            role_registry: Arc::new(RoleRegistry::new()),
            lsp: djinn_lsp::LspManager::new(),
            self_sender: tokio::sync::mpsc::channel(1).0,
            status_tx: tokio::sync::watch::channel(SharedCoordinatorState {
                dispatched: 0,
                recovered: 0,
                epic_throughput: std::collections::HashMap::new(),
                pr_errors: std::collections::HashMap::new(),
                rate_limited_until: None,
            })
            .0,
            dispatch_limit: 50,
            model_priorities: std::collections::HashMap::new(),
            #[cfg(test)]
            test_use_live_credential_resolution: false,
            pr_errors: std::collections::HashMap::new(),
            last_dispatched: std::collections::HashMap::new(),
            inflight_dispatches: std::collections::HashMap::new(),
            provisional_admissions: std::collections::HashMap::new(),
            dispatch_cooldowns: std::collections::HashMap::new(),
            dispatch_failure_streak: std::collections::HashMap::new(),
            breaker_open_backoff_streak: std::collections::HashMap::new(),
            background_work_tracker: BackgroundWorkTracker::default(),
            provider_action_scope: crate::types::ProviderActionScope::new(),
            stranded_ready_source: None,
            doctor_registry: crate::actor::new_doctor_registry_handle(),
            closed_parent_open_children_source: None,
            auto_merge_tracker: AutoMergeTracker::default(),
            consolidation_runner: runner,
            mismatch_scan: crate::doctor::mismatch_scan::MismatchScanCoordinator::new(
                db.clone(),
                crate::events::event_bus_for(tx),
            ),
            last_stale_sweep: StdInstant::now(),
            last_ci_route_sweep: StdInstant::now(),
            last_auto_dispatch_sweep: StdInstant::now(),
            last_proposal_review_sweep: StdInstant::now(),
            last_graph_refresh: StdInstant::now(),
            workload_inventory: None,
            startup_census: None,
            graph_warmer: None,
            mirror: None,
            runtime_ops: None,
            rpc_registry: None,
            prune_tick_counter: 0,
            throughput_events: std::collections::HashMap::new(),
            pr_status_cache: std::collections::HashMap::new(),
            pr_draft_first_seen: std::collections::HashMap::new(),
            review_stuck_sha_first_seen: std::collections::HashMap::new(),
            ci_inconclusive_retriggered: std::collections::HashSet::new(),
            merge_fail_count: std::collections::HashMap::new(),
            auto_approve_attempted: std::collections::HashMap::new(),
            delegated_to_github: std::collections::HashMap::new(),
            conversations_resolved: std::collections::HashMap::new(),
            handled_dequeues: std::collections::HashMap::new(),
            stall_killed: std::collections::HashSet::new(),
            stall_progress_watermark: std::collections::HashMap::new(),
            stall_cancel_streak: std::collections::HashMap::new(),
            stall_extension_count: std::collections::HashMap::new(),
            provider_failure_streak: std::collections::HashMap::new(),
            last_idle_consolidation: None,
            idle_consolidation_cancel: None,
            idle_consolidation_handle: None,
            pr_cleanup_config: PrCleanupConfig::default(),
            worker_lifecycle_config: crate::WorkerLifecycleConfig::default(),
            active_refinements: std::collections::HashMap::new(),
            refinement_sessions: std::collections::HashMap::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hourly_background_tick_invokes_consolidation_runner_for_db_note_group() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());
        let note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm A",
                "Retry storm causes duplicate work during incident recovery.",
                "case",
                "[]",
            )
            .await
            .unwrap();
        let note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm B",
                "Retry storm causes duplicate work during incident recovery.",
                "case",
                "[]",
            )
            .await
            .unwrap();

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_a.id, &session.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_b.id, &session.id)
            .await
            .unwrap();

        let runner = Arc::new(RecordingConsolidationRunner::new());
        let actor = test_actor(&db, &tx, runner.clone());
        run_note_consolidation(&actor.db, &actor.consolidation_runner).await;

        let session_groups = runner.session_groups();
        assert_eq!(session_groups.len(), 1);
        assert_eq!(session_groups[0].0.project_id, project.id);
        assert_eq!(session_groups[0].0.note_type, "case");
        assert_eq!(session_groups[0].1, session.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_consolidation_skips_during_rate_limit_and_resumes_after_clear() {
        clear_suppression_window();

        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let runner = Arc::new(RecordingConsolidationRunner::new());
        let mut actor = test_actor(&db, &tx, runner);

        activate_suppression_window(std::time::Duration::from_secs(30));
        assert!(actor.should_skip_background_llm_work("idle_note_consolidation"));
        assert!(actor.current_rate_limited_until().is_some());
        assert!(actor.idle_consolidation_handle.is_none());

        clear_suppression_window();
        assert!(!actor.should_skip_background_llm_work("idle_note_consolidation"));
        actor.maybe_start_idle_consolidation().await;
        assert!(actor.idle_consolidation_handle.is_some());
        actor.cancel_idle_consolidation();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn below_threshold_clusters_are_noop_for_consolidation_runner() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());
        note_repo
            .create_db_note(
                &project.id,
                "Incident Pattern A",
                "Repeated timeout while syncing cache data.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        note_repo
            .create_db_note(
                &project.id,
                "Incident Pattern B",
                "Repeated timeout while syncing cache data.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let metrics_before = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert!(metrics_before.is_empty());

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group(djinn_db::DbNoteGroup {
                project_id: project.id.clone(),
                note_type: "pattern".to_string(),
                note_count: 2,
            })
            .await
            .unwrap();

        let metrics_after = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert!(metrics_after.is_empty());

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(notes.len(), 2);

        for note in &notes {
            let provenance = consolidation_repo.list_provenance(&note.id).await.unwrap();
            assert!(provenance.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn qualifying_clusters_create_canonical_note_provenance_and_completed_metric() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());

        let note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm A",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm B",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let note_c = note_repo
            .create_db_note(
                &project.id,
                "Retry Storm C",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        for (note_id, overview) in [
            (&note_a.id, "Prefer backoff and idempotent recovery steps."),
            (
                &note_b.id,
                "Throttle retries before cache warmup completes.",
            ),
            (&note_c.id, "Use idempotent jobs plus exponential backoff."),
        ] {
            note_repo
                .update_summaries(
                    note_id,
                    Some("Retry storms amplify duplicate work during recovery."),
                    Some(overview),
                )
                .await
                .unwrap();
        }

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let session_a = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let session_b = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let session_c = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_a.id, &session_a.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_b.id, &session_b.id)
            .await
            .unwrap();
        consolidation_repo
            .add_provenance(&note_c.id, &session_c.id)
            .await
            .unwrap();

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group(djinn_db::DbNoteGroup {
                project_id: project.id.clone(),
                note_type: "pattern".to_string(),
                note_count: 3,
            })
            .await
            .unwrap();

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(notes.len(), 4);
        let canonical = notes
            .iter()
            .find(|note| note.id != note_a.id && note.id != note_b.id && note.id != note_c.id)
            .unwrap();
        assert!(
            canonical
                .title
                .starts_with("Canonical pattern: Retry Storm")
        );
        assert!(canonical.content.contains("## Source notes"));
        assert!(canonical.content.contains(&note_a.permalink));
        assert_eq!(
            canonical.abstract_.as_deref(),
            Some("Retry storms amplify duplicate work during recovery.")
        );
        assert!(canonical.confidence >= 0.65 && canonical.confidence <= 0.8);

        let provenance = consolidation_repo
            .list_provenance(&canonical.id)
            .await
            .unwrap();
        assert_eq!(
            provenance
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                session_a.id.as_str(),
                session_b.id.as_str(),
                session_c.id.as_str()
            ]
        );

        let metrics = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert_eq!(metrics.len(), 1);
        let metric = &metrics[0];
        assert_eq!(metric.status, "completed");
        assert_eq!(metric.scanned_note_count, 3);
        assert_eq!(metric.candidate_cluster_count, 1);
        assert_eq!(metric.consolidated_cluster_count, 1);
        assert_eq!(metric.consolidated_note_count, 1);
        assert_eq!(metric.source_note_count, 3);
        assert!(metric.completed_at.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_scoped_consolidation_excludes_cross_session_notes_and_preserves_metrics() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());

        let session_note_a = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster A",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let session_note_b = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster B",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let session_note_c = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster C",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();
        let cross_session_note = note_repo
            .create_db_note(
                &project.id,
                "Retry Cluster D",
                "Repeated retry storm during incident recovery.",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        for (note_id, overview) in [
            (
                &session_note_a.id,
                "Prefer backoff and idempotent recovery steps.",
            ),
            (
                &session_note_b.id,
                "Throttle retries before cache warmup completes.",
            ),
            (
                &session_note_c.id,
                "Use idempotent jobs plus exponential backoff.",
            ),
            (
                &cross_session_note.id,
                "A later session found the same retry pattern independently.",
            ),
        ] {
            note_repo
                .update_summaries(
                    note_id,
                    Some("Retry storms amplify duplicate work during recovery."),
                    Some(overview),
                )
                .await
                .unwrap();
        }

        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let source_session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let later_session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        for note_id in [&session_note_a.id, &session_note_b.id, &session_note_c.id] {
            consolidation_repo
                .add_provenance(note_id, &source_session.id)
                .await
                .unwrap();
        }
        consolidation_repo
            .add_provenance(&cross_session_note.id, &later_session.id)
            .await
            .unwrap();

        let runner = Arc::new(DbConsolidationRunner::new(db.clone()));
        runner
            .run_for_group_in_session(
                djinn_db::DbNoteGroup {
                    project_id: project.id.clone(),
                    note_type: "pattern".to_string(),
                    note_count: 3,
                },
                source_session.id.clone(),
            )
            .await
            .unwrap();

        let notes = consolidation_repo
            .list_db_notes_in_group(&project.id, "pattern")
            .await
            .unwrap();
        assert_eq!(notes.len(), 5);
        let canonical = notes
            .iter()
            .find(|note| {
                ![
                    &session_note_a.id,
                    &session_note_b.id,
                    &session_note_c.id,
                    &cross_session_note.id,
                ]
                .contains(&&note.id)
            })
            .unwrap();
        assert!(canonical.content.contains(&session_note_a.permalink));
        assert!(canonical.content.contains(&session_note_b.permalink));
        assert!(canonical.content.contains(&session_note_c.permalink));
        assert!(!canonical.content.contains(&cross_session_note.permalink));

        let provenance = consolidation_repo
            .list_provenance(&canonical.id)
            .await
            .unwrap();
        assert_eq!(
            provenance
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![source_session.id.as_str()]
        );

        let metrics = consolidation_repo
            .list_run_metrics(&project.id, Some("pattern"), 20)
            .await
            .unwrap();
        assert_eq!(metrics.len(), 1);
        let metric = &metrics[0];
        assert_eq!(metric.status, "completed");
        assert_eq!(metric.scanned_note_count, 3);
        assert_eq!(metric.candidate_cluster_count, 1);
        assert_eq!(metric.consolidated_cluster_count, 1);
        assert_eq!(metric.consolidated_note_count, 1);
        assert_eq!(metric.source_note_count, 3);
        assert!(metric.completed_at.is_some());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // t5rn AC6 — enablement gate, one commit per run, deferred clusters
    // ═══════════════════════════════════════════════════════════════════════

    async fn note_status(db: &Database, note_id: &str) -> Option<String> {
        let repo = NoteRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        repo.get(note_id).await.unwrap().map(|note| note.status)
    }

    /// Count notes whose *immutable* creation revision is consolidation
    /// attributed — the authoritative canonical identity, not the display tag.
    async fn canonical_note_count(db: &Database, project_id: &str, note_type: &str) -> usize {
        let repo = NoteConsolidationRepository::new(db.clone());
        let notes = repo
            .list_db_notes_in_group(project_id, note_type)
            .await
            .unwrap();
        let mut count = 0usize;
        for note in notes {
            if repo.is_consolidation_canonical(&note.id).await.unwrap() {
                count += 1;
            }
        }
        count
    }

    /// Build a dense, fully-eligible partition of `count` similar notes.
    async fn dense_partition(
        db: &Database,
        tx: &broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
        project_id: &str,
        count: usize,
    ) -> (String, Vec<String>) {
        let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(tx));
        let consolidation_repo = NoteConsolidationRepository::new(db.clone());
        let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(tx));
        let session = session_repo
            .create(CreateSessionParams {
                project_id,
                task_id: None,
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let mut note_ids = Vec::with_capacity(count);
        for index in 0..count {
            let note = note_repo
                .create_db_note(
                    project_id,
                    &format!("Retry Storm Variant {index}"),
                    "Retry storms amplify duplicate recovery work during incident recovery.",
                    "pattern",
                    "[]",
                )
                .await
                .unwrap();
            consolidation_repo
                .add_provenance(&note.id, &session.id)
                .await
                .unwrap();
            note_ids.push(note.id);
        }
        (session.id, note_ids)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_writes_default_off_and_report_defers_every_cluster() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let (session, note_ids) = dense_partition(&db, &tx, &project.id, 12).await;

        // The shipped default gate is off.
        assert!(!ConsolidationEnablement::default().canonical_writes_enabled);

        let config = ConsolidationEnablement {
            canonical_writes_enabled: false,
            project_id: Some(project.id.clone()),
            session_id: Some(session.clone()),
            note_type: Some("pattern".to_owned()),
            // Relaxed so this fixture's real `ts_rank` scores clear it; the
            // production default remains `CONSOLIDATION_DEFAULT_SCORE_THRESHOLD`.
            score_threshold: 1e-6,
        };
        let report = run_bounded_consolidation(&db, &config).await.unwrap();

        assert!(
            report.write_result.is_none(),
            "a disabled run must not write"
        );
        assert!(report.rejection_reason.is_none());
        assert_eq!(report.input_count, 12);
        assert!(
            !report.deferred_clusters.is_empty(),
            "the fixture must produce at least one qualifying cluster to defer"
        );
        assert_eq!(
            report.deferred_clusters.len(),
            report.qualifying_cluster_count
        );

        // Nothing was mutated and no canonical exists.
        assert_eq!(canonical_note_count(&db, &project.id, "pattern").await, 0);
        for note_id in &note_ids {
            assert_eq!(note_status(&db, note_id).await.as_deref(), Some("active"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_exact_one_partition_keys_are_rejected_before_synthesis() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let (session, note_ids) = dense_partition(&db, &tx, &project.id, 4).await;

        let invalid = [
            (None, Some(session.clone()), Some("pattern".to_owned())),
            (Some(project.id.clone()), None, Some("pattern".to_owned())),
            (Some(project.id.clone()), Some(session.clone()), None),
            (
                Some(project.id.clone()),
                Some(session.clone()),
                Some("design".to_owned()),
            ),
            (
                Some(project.id.clone()),
                Some(session.clone()),
                Some("case,pattern".to_owned()),
            ),
            (
                Some("*".to_owned()),
                Some(session.clone()),
                Some("pattern".to_owned()),
            ),
        ];

        for (project_id, session_id, note_type) in invalid {
            let config = ConsolidationEnablement {
                canonical_writes_enabled: true,
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                note_type: note_type.clone(),
                score_threshold: 1e-6,
            };
            let report = run_bounded_consolidation(&db, &config).await.unwrap();
            assert!(
                report.rejection_reason.is_some(),
                "expected rejection for {project_id:?}/{session_id:?}/{note_type:?}"
            );
            assert!(report.write_result.is_none());
            assert!(report.requested_partition.is_none());
        }

        assert_eq!(canonical_note_count(&db, &project.id, "pattern").await, 0);
        for note_id in &note_ids {
            assert_eq!(note_status(&db, note_id).await.as_deref(), Some("active"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn enabled_dense_run_commits_only_its_first_cluster_and_defers_the_rest() {
        let db = test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(256);
        let project = test_helpers::create_test_project(&db).await;
        let (session, note_ids) = dense_partition(
            &db,
            &tx,
            &project.id,
            djinn_db::CONSOLIDATION_MAX_PARTITION_INPUTS,
        )
        .await;

        let config = ConsolidationEnablement {
            canonical_writes_enabled: true,
            project_id: Some(project.id.clone()),
            session_id: Some(session.clone()),
            note_type: Some("pattern".to_owned()),
            score_threshold: 1e-6,
        };
        let report = run_bounded_consolidation(&db, &config).await.unwrap();

        assert!(report.rejection_reason.is_none());
        assert!(report.conflict_reason.is_none());
        assert_eq!(
            report.input_count,
            djinn_db::CONSOLIDATION_MAX_PARTITION_INPUTS
        );
        assert!(report.admission_comparisons <= djinn_db::CONSOLIDATION_MAX_ADMISSION_COMPARISONS);

        let write_result = report
            .write_result
            .as_ref()
            .expect("an enabled dense run commits its first qualifying cluster");
        let committed_sources = write_result
            .supersedes_source_note_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert!(committed_sources.len() >= djinn_db::CONSOLIDATION_MIN_CLUSTER_SOURCES);
        assert!(committed_sources.len() <= djinn_db::CONSOLIDATION_MAX_CLUSTER_SOURCES);
        assert!(!write_result.canonical_note_id.is_empty());
        assert!(!write_result.consolidation_attempt_id.is_empty());
        assert!(
            write_result
                .canonical_provenance_session_ids
                .contains(&session)
        );
        assert!(
            write_result
                .final_source_statuses
                .iter()
                .all(|(_, status)| status == "superseded")
        );

        // Exactly one canonical transaction ran.
        assert_eq!(canonical_note_count(&db, &project.id, "pattern").await, 1);

        // Every other source is untouched.
        for note_id in &note_ids {
            let expected = if committed_sources.contains(note_id) {
                "superseded"
            } else {
                "active"
            };
            assert_eq!(
                note_status(&db, note_id).await.as_deref(),
                Some(expected),
                "note {note_id} should be {expected}"
            );
        }

        // Every remaining qualifying cluster is reported and disjoint from the
        // committed one.
        assert!(
            !report.deferred_clusters.is_empty(),
            "a dense 200-input run must defer the clusters it did not commit"
        );
        assert_eq!(
            report.deferred_clusters.len(),
            report.qualifying_cluster_count - 1
        );
        for deferred in &report.deferred_clusters {
            assert!(
                deferred.iter().all(|id| !committed_sources.contains(id)),
                "deferred clusters must not overlap the committed cluster"
            );
        }
    }
}
