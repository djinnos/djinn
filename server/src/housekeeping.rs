use std::path::Path;
use std::time::Duration;

use djinn_core::models::Project;
use djinn_db::{NoteRepository, ProjectRepository};
use tokio::time::{Interval, MissedTickBehavior};

use crate::server::AppState;

const DEFAULT_HOUSEKEEPING_INTERVAL_SECS: u64 = 60 * 60;
const ORPHAN_TAG: &str = "orphan";
const BROKEN_WIKILINK_MIN_SCORE: f64 = 0.0;
const HOUSEKEEPING_INTERVAL_ENV: &str = "DJINN_HOUSEKEEPING_INTERVAL_SECS";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HousekeepingConfig {
    pub interval: Duration,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HousekeepingTickReport {
    pub project_reports: Vec<ProjectHousekeepingReport>,
    pub total_pruned_associations: u64,
    pub total_orphan_notes_flagged: u64,
    pub total_rebuilt_content_hashes: u64,
    pub total_repaired_broken_wikilinks: u64,
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

        Self {
            project_reports,
            total_pruned_associations,
            total_orphan_notes_flagged,
            total_rebuilt_content_hashes,
            total_repaired_broken_wikilinks,
        }
    }
}

pub fn spawn(state: AppState) {
    let config = HousekeepingConfig::from_env();
    let cancel = state.cancel().clone();
    tokio::spawn(async move {
        let mut ticker = housekeeping_ticker(config.interval);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(error) = run_tick(&state).await {
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

async fn run_tick(state: &AppState) -> anyhow::Result<HousekeepingTickReport> {
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let projects = project_repo.list().await?;

    let mut project_reports = Vec::with_capacity(projects.len());
    for project in projects {
        let report = run_project_housekeeping(state, &project).await?;

        tracing::info!(
            project_id = %report.project_id,
            project_name = %report.project_name,
            pruned_associations = report.pruned_associations,
            orphan_notes_flagged = report.orphan_notes_flagged,
            rebuilt_content_hashes = report.rebuilt_content_hashes,
            repaired_broken_wikilinks = report.repaired_broken_wikilinks,
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
        "knowledge base housekeeping tick summary"
    );

    Ok(report)
}

async fn run_project_housekeeping(
    state: &AppState,
    project: &Project,
) -> anyhow::Result<ProjectHousekeepingReport> {
    let path = Path::new(&project.path);
    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
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

    Ok(ProjectHousekeepingReport {
        project_id: project.id.clone(),
        project_name: project.name.clone(),
        pruned_associations,
        orphan_notes_flagged,
        rebuilt_content_hashes,
        repaired_broken_wikilinks,
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

    use djinn_db::test_support::build_multi_project_housekeeping_fixture;
    use futures::StreamExt;
    use tokio_util::sync::CancellationToken;

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
            },
            ProjectHousekeepingReport {
                project_id: "project-b".to_string(),
                project_name: "B".to_string(),
                pruned_associations: 10,
                orphan_notes_flagged: 20,
                rebuilt_content_hashes: 30,
                repaired_broken_wikilinks: 40,
            },
        ]);

        assert_eq!(report.project_reports.len(), 2);
        assert_eq!(report.total_pruned_associations, 11);
        assert_eq!(report.total_orphan_notes_flagged, 22);
        assert_eq!(report.total_rebuilt_content_hashes, 33);
        assert_eq!(report.total_repaired_broken_wikilinks, 44);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_tick_uses_exported_fixture_for_nonzero_broken_wikilink_repairs() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let fixture = build_multi_project_housekeeping_fixture(&db).await;
        let state = AppState::new(db.clone(), CancellationToken::new());

        let before_counts: HashMap<String, i64> = fixture
            .projects
            .iter()
            .map(|fixture_project| async {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM note_links WHERE source_id = ?1 AND target_id IS NULL",
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

        let report = run_tick(&state).await.unwrap();

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

            let repaired_content: String =
                sqlx::query_scalar("SELECT content FROM notes WHERE id = ?1")
                    .bind(&fixture_project.repaired_source_note_id)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            let repaired_target_title: String =
                sqlx::query_scalar("SELECT title FROM notes WHERE id = ?1")
                    .bind(&fixture_project.repaired_target_note_id)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();

            if project_report.repaired_broken_wikilinks == 0 {
                let search_results = NoteRepository::new(db.clone(), state.event_bus())
                    .search(
                        &fixture_project.project.id,
                        &repaired_target_title,
                        None,
                        None,
                        None,
                        3,
                    )
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
                "SELECT COUNT(*) FROM note_links WHERE source_id = ?1 AND target_id IS NULL",
            )
            .bind(&fixture_project.repaired_source_note_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(unresolved_after, 0);

            let orphan_tags: String = sqlx::query_scalar("SELECT tags FROM notes WHERE id = ?1")
                .bind(&fixture_project.orphan_note_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
            assert_eq!(orphan_tags, "[\"orphan\"]");

            let rebuilt_hashes: Vec<Option<String>> =
                sqlx::query_scalar("SELECT content_hash FROM notes WHERE id IN (?1, ?2)")
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

        assert_eq!(report.total_pruned_associations, expected_total_pruned);
        assert_eq!(report.total_orphan_notes_flagged, expected_total_orphans);
        assert_eq!(report.total_rebuilt_content_hashes, expected_total_hashes);
        assert_eq!(
            report.total_repaired_broken_wikilinks,
            expected_total_repairs
        );
        assert!(report.total_repaired_broken_wikilinks > 0);
    }
}
