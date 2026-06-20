use std::time::Duration;

use djinn_core::events::EventBus;
use djinn_core::models::Project;
use tokio::time::{Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::Database;
use crate::repositories::note::{
    CreateConsolidationRunMetric, NoteConsolidationRepository, NoteRepository,
};
use crate::repositories::project::ProjectRepository;

const DEFAULT_HOUSEKEEPING_INTERVAL_SECS: u64 = 60 * 60;
const ORPHAN_TAG: &str = "orphan";
const BROKEN_WIKILINK_MIN_SCORE: f64 = 0.0;
const HOUSEKEEPING_INTERVAL_ENV: &str = "DJINN_HOUSEKEEPING_INTERVAL_SECS";
/// Default archive-candidate access window (days) used by the per-project
/// housekeeping sweep when [`ARCHIVE_WINDOW_ENV`] is absent or invalid.
/// Distinct from the decay window so decay and archive cadences can be tuned
/// independently — archive is strictly stricter (no recent access).
const DEFAULT_ARCHIVE_WINDOW_DAYS: u32 = 60;
/// Env-var name consumed by [`parse_archive_window_days`] for the housekeeping
/// archive sweep. Kept in this module so the scheduler wiring owns the
/// operator-facing knob it passes to `archive_audit_candidates`.
const ARCHIVE_WINDOW_ENV: &str = "DJINN_LIFECYCLE_ARCHIVE_WINDOW_DAYS";
/// Env-gated opt-in: when set to "1" (true), the periodic housekeeping tick
/// also runs a one-shot dry-run retrieval-anchor backfill report (proposal
/// counts and proposed anchors, no writes). Apply mode is never invoked
/// automatically — operators must call the explicit entry point
/// [`run_backfill_anchors_for_all_projects`] to write.
const BACKFILL_DRY_RUN_ENV: &str = "DJINN_HOUSEKEEPING_BACKFILL_ANCHORS_DRY_RUN";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HousekeepingConfig {
    pub interval: Duration,
}

/// Explicit, re-runnable lifecycle sweep entry point for operators/tests. The
/// default option set is dry-run, which only projects decay/archive impact. Set
/// `dry_run = false` to apply the same reversible decay and archive actions used
/// by periodic housekeeping. This path never calls `memory_delete`.
pub async fn run_lifecycle_sweep_for_all_projects(
    db: &Database,
    event_bus: &EventBus,
    options: LifecycleSweepOptions,
) -> anyhow::Result<LifecycleSweepReport> {
    let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
    let projects = project_repo.list().await?;
    let note_repo = NoteRepository::new(db.clone(), event_bus.clone());

    let mut project_reports = Vec::with_capacity(projects.len());
    for project in projects {
        let projection = note_repo
            .lifecycle_sweep_projection(
                &project.id,
                options.decay_window_days,
                options.archive_window_days,
            )
            .await?;

        let (decayed_candidates, archive_candidates, already_archived) = if options.dry_run {
            (
                projection.decayed_candidates,
                projection.archive_candidates,
                projection.already_archived,
            )
        } else {
            let decayed = note_repo
                .decay_stale_extracted_notes(&project.id, options.decay_window_days)
                .await?;
            let archived = note_repo
                .archive_audit_candidates(&project.id, options.archive_window_days)
                .await?;
            let post_projection = note_repo
                .lifecycle_sweep_projection(
                    &project.id,
                    options.decay_window_days,
                    options.archive_window_days,
                )
                .await?;
            (decayed, archived, post_projection.already_archived)
        };

        tracing::info!(
            project_id = %project.id,
            project_name = %project.name,
            decayed_candidates,
            archive_candidates,
            already_archived,
            dry_run = options.dry_run,
            "lifecycle sweep project report"
        );

        project_reports.push(ProjectLifecycleSweepReport {
            project_id: project.id,
            project_name: project.name,
            decayed_candidates,
            archive_candidates,
            already_archived,
        });
    }

    let report = LifecycleSweepReport::new(project_reports, options.dry_run);
    tracing::info!(
        project_count = report.projects.len(),
        total_decayed_candidates = report.total_decayed_candidates,
        total_archive_candidates = report.total_archive_candidates,
        total_already_archived = report.total_already_archived,
        dry_run = report.dry_run,
        "lifecycle sweep summary"
    );
    Ok(report)
}

impl HousekeepingConfig {
    pub(crate) fn from_env() -> Self {
        Self {
            interval: parse_housekeeping_interval(
                std::env::var(HOUSEKEEPING_INTERVAL_ENV).ok().as_deref(),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectHousekeepingReport {
    pub project_id: String,
    pub project_name: String,
    pub pruned_associations: u64,
    pub orphan_notes_flagged: u64,
    pub rebuilt_content_hashes: u64,
    pub repaired_broken_wikilinks: u64,
    /// Extracted notes (`case`/`pattern`/`pitfall`) whose confidence crossed
    /// below `STALE_CITATION` during this tick's decay pass.
    pub decayed_notes: u64,
    /// Extracted notes flipped from `active` to `archived` by the
    /// ADR-054 archive-candidate sweep this tick. The sweep runs after decay
    /// and uses [`crate::repositories::note::lifecycle::ARCHIVE_WINDOW_ENV`]
    /// (default 60 days). Only reversible status transitions are counted;
    /// already-archived or otherwise non-active candidates are no-ops and do
    /// not increment this counter.
    pub archived_notes: u64,
    /// Source notes superseded by lifecycle/consolidation work attributed to
    /// this housekeeping sweep. The periodic lifecycle sweep itself does not
    /// create canonical notes, so this remains zero unless a future substrate
    /// adds a per-tick supersession pass.
    pub superseded_source_notes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HousekeepingTickReport {
    pub project_reports: Vec<ProjectHousekeepingReport>,
    pub total_pruned_associations: u64,
    pub total_orphan_notes_flagged: u64,
    pub total_rebuilt_content_hashes: u64,
    pub total_repaired_broken_wikilinks: u64,
    pub total_decayed_notes: u64,
    pub total_archived_notes: u64,
    pub total_superseded_source_notes: u64,
}

impl HousekeepingTickReport {
    fn new(project_reports: Vec<ProjectHousekeepingReport>) -> Self {
        let total_pruned_associations = project_reports
            .iter()
            .map(|report| report.pruned_associations)
            .sum();
        let total_orphan_notes_flagged = project_reports
            .iter()
            .map(|report| report.orphan_notes_flagged)
            .sum();
        let total_rebuilt_content_hashes = project_reports
            .iter()
            .map(|report| report.rebuilt_content_hashes)
            .sum();
        let total_repaired_broken_wikilinks = project_reports
            .iter()
            .map(|report| report.repaired_broken_wikilinks)
            .sum();
        let total_decayed_notes = project_reports
            .iter()
            .map(|report| report.decayed_notes)
            .sum();
        let total_archived_notes = project_reports
            .iter()
            .map(|report| report.archived_notes)
            .sum();
        let total_superseded_source_notes = project_reports
            .iter()
            .map(|report| report.superseded_source_notes)
            .sum();

        Self {
            project_reports,
            total_pruned_associations,
            total_orphan_notes_flagged,
            total_rebuilt_content_hashes,
            total_repaired_broken_wikilinks,
            total_decayed_notes,
            total_archived_notes,
            total_superseded_source_notes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleSweepOptions {
    pub dry_run: bool,
    pub decay_window_days: u32,
    pub archive_window_days: u32,
}

impl Default for LifecycleSweepOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            decay_window_days: crate::repositories::note::lifecycle::DEFAULT_DECAY_WINDOW_DAYS,
            archive_window_days: DEFAULT_ARCHIVE_WINDOW_DAYS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectLifecycleSweepReport {
    pub project_id: String,
    pub project_name: String,
    pub decayed_candidates: u64,
    pub archive_candidates: u64,
    pub already_archived: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleSweepReport {
    pub projects: Vec<ProjectLifecycleSweepReport>,
    pub total_decayed_candidates: u64,
    pub total_archive_candidates: u64,
    pub total_already_archived: u64,
    pub dry_run: bool,
}

impl LifecycleSweepReport {
    fn new(projects: Vec<ProjectLifecycleSweepReport>, dry_run: bool) -> Self {
        let total_decayed_candidates = projects.iter().map(|p| p.decayed_candidates).sum();
        let total_archive_candidates = projects.iter().map(|p| p.archive_candidates).sum();
        let total_already_archived = projects.iter().map(|p| p.already_archived).sum();
        Self {
            projects,
            total_decayed_candidates,
            total_archive_candidates,
            total_already_archived,
            dry_run,
        }
    }
}

/// Per-project summary of a retrieval-anchor backfill run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectBackfillReport {
    pub project_id: String,
    pub project_name: String,
    pub report: crate::BackfillRetrievalAnchorReport,
}

/// Cross-project summary of a backfill run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackfillTickReport {
    pub projects: Vec<ProjectBackfillReport>,
    pub total_candidates: usize,
    pub total_updated: usize,
    pub total_skipped_blank_proposal: usize,
    pub dry_run: bool,
}

impl BackfillTickReport {
    fn new(projects: Vec<ProjectBackfillReport>, dry_run: bool) -> Self {
        let total_candidates = projects.iter().map(|p| p.report.candidate_count).sum();
        let total_updated = projects.iter().map(|p| p.report.updated).sum();
        let total_skipped_blank_proposal = projects
            .iter()
            .map(|p| p.report.skipped_blank_proposal)
            .sum();
        Self {
            projects,
            total_candidates,
            total_updated,
            total_skipped_blank_proposal,
            dry_run,
        }
    }
}

fn backfill_dry_run_enabled() -> bool {
    matches!(
        std::env::var(BACKFILL_DRY_RUN_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// Explicit, re-runnable entry point: run a retrieval-anchor backfill across
/// every project in the database. `dry_run` defaults to true so the first
/// call is always a report. Set `dry_run = false` to apply. Idempotent:
/// notes that already have a non-empty anchor are skipped unless
/// `force = true`.
pub async fn run_backfill_anchors_for_all_projects(
    db: &Database,
    event_bus: &EventBus,
    options: crate::BackfillRetrievalAnchorOptions,
) -> anyhow::Result<BackfillTickReport> {
    let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
    let projects = project_repo.list().await?;
    let note_repo = NoteRepository::new(db.clone(), event_bus.clone());

    let mut project_reports = Vec::with_capacity(projects.len());
    for project in projects {
        let report = note_repo
            .backfill_retrieval_anchors(&project.id, options.clone())
            .await?;
        tracing::info!(
            project_id = %project.id,
            project_name = %project.name,
            candidate_count = report.candidate_count,
            updated = report.updated,
            skipped_blank_proposal = report.skipped_blank_proposal,
            dry_run = report.dry_run,
            "retrieval-anchor backfill project report"
        );
        project_reports.push(ProjectBackfillReport {
            project_id: project.id,
            project_name: project.name,
            report,
        });
    }

    let tick = BackfillTickReport::new(project_reports, options.dry_run);
    tracing::info!(
        project_count = tick.projects.len(),
        total_candidates = tick.total_candidates,
        total_updated = tick.total_updated,
        total_skipped_blank_proposal = tick.total_skipped_blank_proposal,
        dry_run = tick.dry_run,
        "retrieval-anchor backfill tick summary"
    );
    Ok(tick)
}

/// Spawn the periodic knowledge-base housekeeping task. Runs every
/// `DJINN_HOUSEKEEPING_INTERVAL_SECS` seconds (default: 1 hour) until
/// `cancel` fires. Prunes stale associations, flags orphan notes,
/// rebuilds missing content hashes, and repairs broken wikilinks across
/// every registered project.
pub fn spawn(db: Database, event_bus: EventBus, cancel: CancellationToken) {
    let config = HousekeepingConfig::from_env();
    tokio::spawn(async move {
        let mut ticker = housekeeping_ticker(config.interval);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(error) = run_tick(&db, &event_bus).await {
                        tracing::error!(error = %error, "housekeeping tick failed");
                    }
                }
            }
        }
    });
}

fn parse_housekeeping_interval(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_HOUSEKEEPING_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Read [`ARCHIVE_WINDOW_ENV`] and return a positive `u32` window in days,
/// falling back to `caller_default` (or [`DEFAULT_ARCHIVE_WINDOW_DAYS`] if the
/// caller accidentally passes zero) when the env var is unset or invalid.
/// This mirrors the existing housekeeping env-override pattern: resolve the
/// operator-facing configuration at the tick boundary, then pass the concrete
/// value into the repository sweep.
fn parse_archive_window_days(caller_default: u32) -> u32 {
    std::env::var(ARCHIVE_WINDOW_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .or_else(|| (caller_default > 0).then_some(caller_default))
        .unwrap_or(DEFAULT_ARCHIVE_WINDOW_DAYS)
}

fn housekeeping_ticker(interval: Duration) -> Interval {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}

#[cfg(test)]
fn ticker_missed_tick_behavior(interval: Duration) -> MissedTickBehavior {
    let ticker = housekeeping_ticker(interval);
    ticker.missed_tick_behavior()
}

async fn run_tick(db: &Database, event_bus: &EventBus) -> anyhow::Result<HousekeepingTickReport> {
    let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
    let projects = project_repo.list().await?;

    let mut project_reports = Vec::with_capacity(projects.len());
    for project in projects {
        let report = run_project_housekeeping(db, event_bus, &project).await?;

        tracing::info!(
            project_id = %report.project_id,
            project_name = %report.project_name,
            pruned_associations = report.pruned_associations,
            orphan_notes_flagged = report.orphan_notes_flagged,
            rebuilt_content_hashes = report.rebuilt_content_hashes,
            repaired_broken_wikilinks = report.repaired_broken_wikilinks,
            decayed_notes = report.decayed_notes,
            archived_notes = report.archived_notes,
            "knowledge base housekeeping project report"
        );

        project_reports.push(report);
    }

    let report = HousekeepingTickReport::new(project_reports);
    tracing::info!(
        project_count = report.project_reports.len(),
        total_pruned_associations = report.total_pruned_associations,
        total_orphan_notes_flagged = report.total_orphan_notes_flagged,
        total_rebuilt_content_hashes = report.total_rebuilt_content_hashes,
        total_repaired_broken_wikilinks = report.total_repaired_broken_wikilinks,
        total_decayed_notes = report.total_decayed_notes,
        total_archived_notes = report.total_archived_notes,
        total_superseded_source_notes = report.total_superseded_source_notes,
        "knowledge base housekeeping tick summary"
    );

    // Opt-in retrieval-anchor backfill (DRY RUN only). Operators wanting
    // apply-mode must call `run_backfill_anchors_for_all_projects` directly
    // — the periodic tick never writes. The env flag exists so an operator
    // can keep an eye on the candidate count in production logs without
    // standing up a separate control-plane call.
    if backfill_dry_run_enabled() {
        match run_backfill_anchors_for_all_projects(
            db,
            event_bus,
            crate::BackfillRetrievalAnchorOptions {
                dry_run: true,
                ..crate::BackfillRetrievalAnchorOptions::default()
            },
        )
        .await
        {
            Ok(_tick) => {}
            Err(error) => tracing::warn!(
                %error,
                "retrieval-anchor dry-run backfill failed; continuing periodic housekeeping"
            ),
        }
    }

    Ok(report)
}

async fn run_project_housekeeping(
    db: &Database,
    event_bus: &EventBus,
    project: &Project,
) -> anyhow::Result<ProjectHousekeepingReport> {
    let path_buf = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
    let path = path_buf.as_path();
    let note_repo = NoteRepository::new(db.clone(), event_bus.clone());
    let pruned_associations = note_repo.prune_associations(&project.id).await?;
    let orphan_notes_flagged = note_repo
        .flag_orphan_notes(&project.id, path, ORPHAN_TAG)
        .await?;
    let rebuilt_content_hashes = note_repo
        .rebuild_missing_content_hashes(&project.id)
        .await?;
    let repaired_broken_wikilinks = note_repo
        .repair_broken_wikilinks(&project.id, path, BROKEN_WIKILINK_MIN_SCORE)
        .await?;
    let decayed_notes = note_repo
        .decay_stale_extracted_notes(
            &project.id,
            crate::repositories::note::lifecycle::DEFAULT_DECAY_WINDOW_DAYS,
        )
        .await?;
    // Archive sweep runs after the decay step and reuses the existing
    // hourly scheduler — no new tick is added here. The repository call
    // performs a reversible `active` → `archived` status flip only; it
    // never hard-deletes note rows. The window is resolved from
    // `DJINN_LIFECYCLE_ARCHIVE_WINDOW_DAYS` (default 60) here at the
    // housekeeping boundary, matching the env-override pattern used by the
    // other tick configuration.
    let archived_notes = note_repo
        .archive_audit_candidates(
            &project.id,
            parse_archive_window_days(DEFAULT_ARCHIVE_WINDOW_DAYS),
        )
        .await?;
    let superseded_source_notes = 0_u64;

    let total_notes_seen_in_sweep: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM notes WHERE project_id = $1"#)
            .bind(&project.id)
            .fetch_one(db.pool())
            .await?;
    let completed_at: String = sqlx::query_scalar(
        r#"SELECT to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
    )
    .fetch_one(db.pool())
    .await?;
    NoteConsolidationRepository::new(db.clone())
        .create_run_metric(CreateConsolidationRunMetric {
            project_id: &project.id,
            note_type: "lifecycle_sweep",
            status: "completed",
            scanned_note_count: total_notes_seen_in_sweep,
            candidate_cluster_count: 0,
            consolidated_cluster_count: 0,
            consolidated_note_count: 0,
            source_note_count: 0,
            decayed_note_count: decayed_notes as i64,
            archived_note_count: archived_notes as i64,
            superseded_source_note_count: superseded_source_notes as i64,
            admission_dropped_note_count: 0,
            started_at: &completed_at,
            completed_at: Some(&completed_at),
            error_message: None,
        })
        .await?;

    Ok(ProjectHousekeepingReport {
        project_id: project.id.clone(),
        project_name: project.name.clone(),
        pruned_associations,
        orphan_notes_flagged,
        rebuilt_content_hashes,
        repaired_broken_wikilinks,
        decayed_notes,
        archived_notes,
        superseded_source_notes,
    })
}

#[cfg(test)]
pub(crate) fn merge_orphan_tag(tags_json: &str, orphan_tag: &str) -> String {
    let mut tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    tags.retain(|tag| seen.insert(tag.clone()));
    if !tags.iter().any(|tag| tag == orphan_tag) {
        tags.push(orphan_tag.to_string());
    }
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::NoteSearchParams;
    use crate::repositories::test_support::{
        build_multi_project_housekeeping_fixture, event_bus_for, make_project,
    };
    use djinn_core::events::EventBus;
    use futures::StreamExt;
    use tokio::sync::broadcast;

    use super::*;

    #[test]
    fn merge_orphan_tag_adds_missing_tag_once() {
        assert_eq!(merge_orphan_tag("[]", "orphan"), "[\"orphan\"]");
        assert_eq!(
            merge_orphan_tag("[\"a\",\"orphan\"]", "orphan"),
            "[\"a\",\"orphan\"]"
        );
    }

    #[test]
    fn housekeeping_interval_uses_positive_override() {
        assert_eq!(
            parse_housekeeping_interval(Some("15")),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn housekeeping_interval_falls_back_for_missing_invalid_or_zero_values() {
        let fallback = Duration::from_secs(DEFAULT_HOUSEKEEPING_INTERVAL_SECS);
        assert_eq!(parse_housekeeping_interval(None), fallback);
        assert_eq!(parse_housekeeping_interval(Some("invalid")), fallback);
        assert_eq!(parse_housekeeping_interval(Some("0")), fallback);
    }

    #[tokio::test]
    async fn housekeeping_ticker_uses_skip_missed_tick_behavior() {
        assert_eq!(
            ticker_missed_tick_behavior(Duration::from_secs(60)),
            MissedTickBehavior::Skip
        );
    }

    #[test]
    fn housekeeping_tick_report_aggregates_project_totals() {
        let report = HousekeepingTickReport::new(vec![
            ProjectHousekeepingReport {
                project_id: "project-a".to_string(),
                project_name: "A".to_string(),
                pruned_associations: 1,
                orphan_notes_flagged: 2,
                rebuilt_content_hashes: 3,
                repaired_broken_wikilinks: 4,
                decayed_notes: 5,
                archived_notes: 6,
                superseded_source_notes: 7,
            },
            ProjectHousekeepingReport {
                project_id: "project-b".to_string(),
                project_name: "B".to_string(),
                pruned_associations: 10,
                orphan_notes_flagged: 20,
                rebuilt_content_hashes: 30,
                repaired_broken_wikilinks: 40,
                decayed_notes: 50,
                archived_notes: 60,
                superseded_source_notes: 70,
            },
        ]);

        assert_eq!(report.project_reports.len(), 2);
        assert_eq!(report.total_pruned_associations, 11);
        assert_eq!(report.total_orphan_notes_flagged, 22);
        assert_eq!(report.total_rebuilt_content_hashes, 33);
        assert_eq!(report.total_repaired_broken_wikilinks, 44);
        assert_eq!(report.total_decayed_notes, 55);
        assert_eq!(report.total_archived_notes, 66);
        assert_eq!(report.total_superseded_source_notes, 77);
    }

    #[test]
    fn parse_archive_window_days_uses_caller_default_when_env_unset() {
        // SAFETY: single-threaded test; no other thread reads this env var
        // here, and we restore the previous state at the end.
        unsafe { std::env::remove_var(ARCHIVE_WINDOW_ENV) };
        assert_eq!(parse_archive_window_days(60), 60);
    }

    #[test]
    fn parse_archive_window_days_falls_back_to_60_when_caller_passes_zero() {
        // SAFETY: single-threaded test; no other thread reads this env var
        // here, and we restore the previous state at the end.
        unsafe { std::env::remove_var(ARCHIVE_WINDOW_ENV) };
        // A zero caller default is treated as "use the module default", which
        // is 60 days. This mirrors the decay-window parser's contract.
        assert_eq!(parse_archive_window_days(0), 60);
    }

    #[test]
    fn parse_archive_window_days_honors_positive_env_override() {
        // SAFETY: single-threaded test; no other thread reads this env var
        // here, and we restore the previous state at the end.
        unsafe { std::env::set_var(ARCHIVE_WINDOW_ENV, "90") };
        // Env wins over caller-provided value.
        assert_eq!(parse_archive_window_days(60), 90);
        unsafe { std::env::remove_var(ARCHIVE_WINDOW_ENV) };
    }

    #[test]
    fn parse_archive_window_days_ignores_zero_and_invalid_env_values() {
        // SAFETY: single-threaded test; no other thread reads this env var
        // here, and we restore the previous state at the end.
        unsafe { std::env::set_var(ARCHIVE_WINDOW_ENV, "0") };
        assert_eq!(parse_archive_window_days(60), 60);
        unsafe { std::env::set_var(ARCHIVE_WINDOW_ENV, "garbage") };
        assert_eq!(parse_archive_window_days(60), 60);
        unsafe { std::env::remove_var(ARCHIVE_WINDOW_ENV) };
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_tick_uses_exported_fixture_for_nonzero_broken_wikilink_repairs() {
        let db = Database::open_in_memory().unwrap();
        let fixture = build_multi_project_housekeeping_fixture(&db).await;
        let event_bus = EventBus::noop();

        let before_counts: HashMap<String, i64> = fixture
            .projects
            .iter()
            .map(|fixture_project| async {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM note_links WHERE source_id = $1 AND target_id IS NULL",
                )
                .bind(&fixture_project.repaired_source_note_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
                (fixture_project.project.id.clone(), count)
            })
            .collect::<futures::stream::FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect();
        assert!(before_counts.values().all(|count| *count > 0));

        let report = run_tick(&db, &event_bus).await.unwrap();

        let actual_by_project: HashMap<_, _> = report
            .project_reports
            .iter()
            .map(|project_report| (project_report.project_id.clone(), project_report))
            .collect();

        assert_eq!(actual_by_project.len(), fixture.projects.len());

        for fixture_project in &fixture.projects {
            let project_report = actual_by_project
                .get(&fixture_project.project.id)
                .copied()
                .expect("missing project report");
            assert_eq!(
                project_report.pruned_associations,
                fixture_project.expected.prune_associations,
            );
            assert_eq!(
                project_report.orphan_notes_flagged,
                fixture_project.expected.flag_orphan_notes,
            );
            assert_eq!(
                project_report.rebuilt_content_hashes,
                fixture_project.expected.rebuild_missing_content_hashes,
            );
            // The shipped multi-project fixture only uses hand-written
            // `reference` notes, so the archive sweep must report zero
            // for every project — the SQL `note_type IN ('case',
            // 'pattern', 'pitfall')` predicate is the safety boundary.
            assert_eq!(
                project_report.archived_notes,
                fixture_project.expected.archive_audit_candidates,
            );

            let repaired_content: String =
                sqlx::query_scalar("SELECT content FROM notes WHERE id = $1")
                    .bind(&fixture_project.repaired_source_note_id)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            let repaired_target_title: String =
                sqlx::query_scalar("SELECT title FROM notes WHERE id = $1")
                    .bind(&fixture_project.repaired_target_note_id)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();

            if project_report.repaired_broken_wikilinks == 0 {
                let search_results = NoteRepository::new(db.clone(), event_bus.clone())
                    .search(NoteSearchParams {
                        project_id: &fixture_project.project.id,
                        query: &repaired_target_title,
                        task_id: None,
                        folder: None,
                        note_type: None,
                        limit: 3,
                        semantic_scores: None,
                        edge_kinds: None,
                    })
                    .await
                    .unwrap();
                panic!(
                    "fixture advertises nonzero broken-wikilink repairs but run_tick returned 0 for project {}. repaired_content={repaired_content:?} target_title={repaired_target_title:?} search_results={search_results:?}",
                    fixture_project.project.id,
                );
            }

            assert_eq!(
                project_report.repaired_broken_wikilinks,
                fixture_project.expected.repair_broken_wikilinks,
            );
            assert!(repaired_content.contains(&format!("[[{repaired_target_title}]]")));

            let unresolved_after: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM note_links WHERE source_id = $1 AND target_id IS NULL",
            )
            .bind(&fixture_project.repaired_source_note_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(unresolved_after, 0);

            let orphan_tags: String =
                sqlx::query_scalar(r#"SELECT tags::text AS "tags!" FROM notes WHERE id = $1"#)
                    .bind(&fixture_project.orphan_note_id)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert_eq!(orphan_tags, "[\"orphan\"]");

            let rebuilt_hashes: Vec<Option<String>> =
                sqlx::query_scalar("SELECT content_hash FROM notes WHERE id IN ($1, $2)")
                    .bind(&fixture_project.legacy_hash_note_ids[0])
                    .bind(&fixture_project.legacy_hash_note_ids[1])
                    .fetch_all(db.pool())
                    .await
                    .unwrap();
            assert!(rebuilt_hashes.iter().all(|hash| hash.is_some()));
        }

        let expected_total_pruned: u64 = fixture
            .projects
            .iter()
            .map(|project| project.expected.prune_associations)
            .sum();
        let expected_total_orphans: u64 = fixture
            .projects
            .iter()
            .map(|project| project.expected.flag_orphan_notes)
            .sum();
        let expected_total_hashes: u64 = fixture
            .projects
            .iter()
            .map(|project| project.expected.rebuild_missing_content_hashes)
            .sum();
        let expected_total_repairs: u64 = fixture
            .projects
            .iter()
            .map(|project| project.expected.repair_broken_wikilinks)
            .sum();
        let expected_total_archived: u64 = fixture
            .projects
            .iter()
            .map(|project| project.expected.archive_audit_candidates)
            .sum();

        assert_eq!(report.total_pruned_associations, expected_total_pruned);
        assert_eq!(report.total_orphan_notes_flagged, expected_total_orphans);
        assert_eq!(report.total_rebuilt_content_hashes, expected_total_hashes);
        assert_eq!(
            report.total_repaired_broken_wikilinks,
            expected_total_repairs
        );
        assert_eq!(report.total_archived_notes, expected_total_archived);
        assert!(report.total_repaired_broken_wikilinks > 0);
    }

    /// Drive `run_project_housekeeping` directly (bypassing `run_tick`'s
    /// project enumeration) so we can stage a project with both a
    /// single-project archive count and a non-zero count that the tick-level
    /// aggregation will sum. This is the "single-project" half of the
    /// acceptance criterion: it proves `archived_notes` is exposed per
    /// project and that the per-project report is wired through the
    /// existing housekeeping call path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_project_housekeeping_reports_archived_notes_for_eligible_candidates() {
        let db = Database::open_in_memory().unwrap();
        let event_bus = EventBus::noop();
        let (tx, _rx) = broadcast::channel(256);
        let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let tmp = crate::database::test_tempdir().unwrap();
        let project = make_project(&db, tmp.path()).await;
        let _ = tmp;

        // ADR-054 archive-candidate shape: short body, "Extracted from session ..."
        // footer, and an extracted note type. With never-accessed and the
        // default 60-day window, the archive sweep should flip it.
        const ARCHIVE_SHAPED_BODY: &str = "One short extracted paragraph.\n\n*Extracted from session 019ed7e1-980f-7ea2-935e-6f5e9fc82c14.*";
        let eligible = note_repo
            .create(
                &project.id,
                "Eligible Archive Candidate",
                ARCHIVE_SHAPED_BODY,
                "case",
                "[]",
            )
            .await
            .unwrap();

        // SAFETY: single-threaded test; clear any leftover override and
        // restore at the end.
        unsafe { std::env::remove_var(ARCHIVE_WINDOW_ENV) };

        let report = run_project_housekeeping(&db, &event_bus, &project)
            .await
            .unwrap();

        assert_eq!(report.archived_notes, 1);

        let status: String = sqlx::query_scalar("SELECT status FROM notes WHERE id = $1")
            .bind(&eligible.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(status, "archived");

        let metric = NoteConsolidationRepository::new(db.clone())
            .list_run_metrics(&project.id, Some("lifecycle_sweep"), 1)
            .await
            .unwrap()
            .pop()
            .expect("housekeeping should persist lifecycle sweep metric");
        assert_eq!(metric.note_type, "lifecycle_sweep");
        assert_eq!(metric.status, "completed");
        assert_eq!(metric.scanned_note_count, 1);
        assert_eq!(metric.decayed_note_count, report.decayed_notes as i64);
        assert_eq!(metric.archived_note_count, 1);
        assert_eq!(metric.superseded_source_note_count, 0);

        let health = note_repo.health(&project.id).await.unwrap();
        assert_eq!(health.lifecycle.active_notes, 0);
        assert_eq!(health.lifecycle.archived_notes, 1);
        assert_eq!(health.recent_sweep.last_sweep_at, metric.completed_at);
        assert_eq!(health.recent_sweep.last_archived_count, 1);
        assert_eq!(
            health.recent_sweep.last_decayed_count,
            report.decayed_notes as i64
        );
        assert_eq!(health.recent_sweep.last_superseded_source_count, 0);
    }

    /// Multi-project aggregation: create two projects with non-zero
    /// archive candidate counts and assert the tick-level
    /// `total_archived_notes` sum is correct. This is the "multi-project"
    /// half of the acceptance criterion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_tick_aggregates_archived_notes_across_multiple_projects() {
        let db = Database::open_in_memory().unwrap();
        let event_bus = EventBus::noop();
        let (tx, _rx) = broadcast::channel(256);
        let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let tmp = crate::database::test_tempdir().unwrap();
        let root = tmp.keep();
        let path_one = root.join("archive-project-one");
        let path_two = root.join("archive-project-two");
        std::fs::create_dir_all(&path_one).unwrap();
        std::fs::create_dir_all(&path_two).unwrap();

        let project_one = make_project(&db, &path_one).await;
        let project_two = make_project(&db, &path_two).await;

        const ARCHIVE_SHAPED_BODY: &str = "One short extracted paragraph.\n\n*Extracted from session 019ed7e1-980f-7ea2-935e-6f5e9fc82c14.*";

        // Project one: two eligible archive candidates.
        let _p1_a = note_repo
            .create(
                &project_one.id,
                "Project One Archive A",
                ARCHIVE_SHAPED_BODY,
                "case",
                "[]",
            )
            .await
            .unwrap();
        let _p1_b = note_repo
            .create(
                &project_one.id,
                "Project One Archive B",
                ARCHIVE_SHAPED_BODY,
                "pitfall",
                "[]",
            )
            .await
            .unwrap();

        // Project two: one eligible archive candidate.
        let _p2_a = note_repo
            .create(
                &project_two.id,
                "Project Two Archive A",
                ARCHIVE_SHAPED_BODY,
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        // SAFETY: single-threaded test; clear any leftover override.
        unsafe { std::env::remove_var(ARCHIVE_WINDOW_ENV) };

        let report = run_tick(&db, &event_bus).await.unwrap();

        let by_project: HashMap<_, _> = report
            .project_reports
            .iter()
            .map(|r| (r.project_id.clone(), r.archived_notes))
            .collect();

        assert_eq!(by_project.get(&project_one.id).copied(), Some(2));
        assert_eq!(by_project.get(&project_two.id).copied(), Some(1));
        // Tick-level aggregation: 2 + 1 = 3 archived notes across both
        // projects.
        assert_eq!(report.total_archived_notes, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_sweep_entry_point_dry_run_apply_and_idempotency() {
        let db = Database::open_in_memory().unwrap();
        let event_bus = EventBus::noop();
        let (tx, _rx) = broadcast::channel(256);
        let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let tmp = crate::database::test_tempdir().unwrap();
        let root = tmp.keep();
        let path_one = root.join("lifecycle-project-one");
        let path_two = root.join("lifecycle-project-two");
        std::fs::create_dir_all(&path_one).unwrap();
        std::fs::create_dir_all(&path_two).unwrap();

        let project_one = make_project(&db, &path_one).await;
        let project_two = make_project(&db, &path_two).await;

        const ARCHIVE_SHAPED_BODY: &str = "One short extracted paragraph.\n\n*Extracted from session 019ed7e1-980f-7ea2-935e-6f5e9fc82c14.*";

        let p1_a = note_repo
            .create(
                &project_one.id,
                "Lifecycle Project One Archive A",
                ARCHIVE_SHAPED_BODY,
                "case",
                "[]",
            )
            .await
            .unwrap();
        let p1_b = note_repo
            .create(
                &project_one.id,
                "Lifecycle Project One Archive B",
                ARCHIVE_SHAPED_BODY,
                "pitfall",
                "[]",
            )
            .await
            .unwrap();
        let p2_a = note_repo
            .create(
                &project_two.id,
                "Lifecycle Project Two Archive A",
                ARCHIVE_SHAPED_BODY,
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let dry_run =
            run_lifecycle_sweep_for_all_projects(&db, &event_bus, LifecycleSweepOptions::default())
                .await
                .unwrap();
        assert!(dry_run.dry_run);
        assert_eq!(dry_run.total_archive_candidates, 3);
        assert_eq!(dry_run.total_already_archived, 0);

        let active_before_apply: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notes WHERE id IN ($1, $2, $3) AND status = 'active'",
        )
        .bind(&p1_a.id)
        .bind(&p1_b.id)
        .bind(&p2_a.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(active_before_apply, 3);

        let applied = run_lifecycle_sweep_for_all_projects(
            &db,
            &event_bus,
            LifecycleSweepOptions {
                dry_run: false,
                ..LifecycleSweepOptions::default()
            },
        )
        .await
        .unwrap();
        assert!(!applied.dry_run);
        assert_eq!(applied.total_archive_candidates, 3);

        let archived_after_apply: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notes WHERE id IN ($1, $2, $3) AND status = 'archived'",
        )
        .bind(&p1_a.id)
        .bind(&p1_b.id)
        .bind(&p2_a.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(archived_after_apply, 3);

        let second_apply = run_lifecycle_sweep_for_all_projects(
            &db,
            &event_bus,
            LifecycleSweepOptions {
                dry_run: false,
                ..LifecycleSweepOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(second_apply.total_archive_candidates, 0);
        assert_eq!(second_apply.total_already_archived, 3);
    }
}
