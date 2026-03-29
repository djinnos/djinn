use std::path::Path;
use std::time::Duration;

use djinn_core::models::Project;
use djinn_db::{NoteRepository, ProjectRepository};
use tokio::time::{Interval, MissedTickBehavior};

use crate::server::AppState;

const DEFAULT_HOUSEKEEPING_INTERVAL_SECS: u64 = 60 * 60;
const ORPHAN_TAG: &str = "orphan";
const BROKEN_WIKILINK_MIN_SCORE: f64 = 20.0;
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
}
