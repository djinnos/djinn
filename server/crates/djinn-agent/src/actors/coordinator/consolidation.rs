use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use djinn_db::{
    CreateCanonicalConsolidatedNote, CreateConsolidationRunMetric, Database, DbNoteGroup,
    NoteConsolidationRepository,
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

    for cluster in qualifying_clusters.iter().copied() {
        let source_session_ids = repo
            .resolve_source_session_ids(&group.project_id, &cluster.note_ids)
            .await?;
        let source_session_refs = source_session_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let synthesized = synthesize_cluster(cluster);

        repo.create_canonical_consolidated_note(CreateCanonicalConsolidatedNote {
            project_id: &group.project_id,
            note_type: &group.note_type,
            title: &synthesized.title,
            content: &synthesized.content,
            tags: CONSOLIDATION_TAGS,
            abstract_: synthesized.abstract_.as_deref(),
            overview: synthesized.overview.as_deref(),
            confidence: synthesized.confidence,
            source_session_ids: &source_session_refs,
            scope_paths: &synthesized.scope_paths,
        })
        .await?;

        consolidated_note_count += 1;
        source_note_count += cluster.note_ids.len() as i64;
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
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 timestamp formatting should succeed")
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
    use crate::actors::coordinator::rules;
    use crate::actors::coordinator::{
        DEFAULT_MODEL_ID, STUCK_INTERVAL, SharedCoordinatorState, VerificationTracker,
    };
    use crate::actors::slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle};
    use crate::roles::RoleRegistry;
    use crate::test_helpers;
    use djinn_db::{CreateSessionParams, NoteRepository, SessionRepository};
    use djinn_provider::catalog::CatalogService;
    use djinn_provider::catalog::health::HealthTracker;
    use djinn_provider::rate_limit::{activate_suppression_window, clear_suppression_window};

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
            lsp: crate::lsp::LspManager::new(),
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
            pr_errors: std::collections::HashMap::new(),
            last_dispatched: std::collections::HashMap::new(),
            dispatch_cooldowns: std::collections::HashMap::new(),
            verification_tracker: VerificationTracker::default(),
            consolidation_runner: runner,
            last_stale_sweep: StdInstant::now(),
            last_auto_dispatch_sweep: StdInstant::now(),
            last_graph_refresh: StdInstant::now(),
            graph_warmer: None,
            mirror: None,
            prune_tick_counter: 0,
            last_patrol_completed: StdInstant::now(),
            next_patrol_interval: rules::DEFAULT_PLANNER_PATROL_INTERVAL,
            throughput_events: std::collections::HashMap::new(),
            escalation_counts: std::collections::HashMap::new(),
            pr_status_cache: std::collections::HashMap::new(),
            pr_draft_first_seen: std::collections::HashMap::new(),
            merge_fail_count: std::collections::HashMap::new(),
            stall_killed: std::collections::HashSet::new(),
            last_idle_consolidation: None,
            idle_consolidation_cancel: None,
            idle_consolidation_handle: None,
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
}
