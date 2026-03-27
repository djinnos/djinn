use std::path::Path;
use std::time::Duration;

use djinn_db::{NoteRepository, ProjectRepository};
use tokio::time::MissedTickBehavior;

use crate::server::AppState;

const DEFAULT_HOUSEKEEPING_INTERVAL_SECS: u64 = 60 * 60;
const ORPHAN_TAG: &str = "orphan";
const BROKEN_WIKILINK_MIN_SCORE: f64 = 20.0;

pub fn spawn(state: AppState) {
    let interval = housekeeping_interval();
    let cancel = state.cancel().clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

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

fn housekeeping_interval() -> Duration {
    let secs = std::env::var("DJINN_HOUSEKEEPING_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_HOUSEKEEPING_INTERVAL_SECS);
    Duration::from_secs(secs)
}

async fn run_tick(state: &AppState) -> anyhow::Result<()> {
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let projects = project_repo.list().await?;

    for project in projects {
        let path = Path::new(&project.path);
        let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
        let pruned = note_repo.prune_associations(&project.id).await?;
        let orphan_flagged = note_repo
            .flag_orphan_notes(&project.id, path, ORPHAN_TAG)
            .await?;
        let hashes_rebuilt = note_repo
            .rebuild_missing_content_hashes(&project.id)
            .await?;
        let wikilinks_repaired = note_repo
            .repair_broken_wikilinks(&project.id, path, BROKEN_WIKILINK_MIN_SCORE)
            .await?;

        tracing::info!(
            project_id = %project.id,
            project_name = %project.name,
            pruned_associations = pruned,
            orphan_notes_flagged = orphan_flagged,
            rebuilt_content_hashes = hashes_rebuilt,
            repaired_broken_wikilinks = wikilinks_repaired,
            "knowledge base housekeeping tick"
        );
    }

    Ok(())
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
}
