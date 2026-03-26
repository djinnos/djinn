use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use djinn_db::{CreateConsolidationRunMetric, Database, DbNoteGroup, NoteConsolidationRepository};

const CONSOLIDATION_MIN_CLUSTER_SIZE: usize = 3;

pub(super) trait ConsolidationRunner: Send + Sync {
    fn run_for_group<'a>(
        &'a self,
        group: DbNoteGroup,
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
            let clusters = repo
                .likely_duplicate_clusters(&group.project_id, &group.note_type)
                .await?;
            if clusters
                .iter()
                .all(|cluster| cluster.note_ids.len() < CONSOLIDATION_MIN_CLUSTER_SIZE)
            {
                return Ok(());
            }

            let now = "1970-01-01T00:00:00Z".to_string();
            let qualifying_clusters = clusters
                .iter()
                .filter(|cluster| cluster.note_ids.len() >= CONSOLIDATION_MIN_CLUSTER_SIZE)
                .collect::<Vec<_>>();

            repo.create_run_metric(CreateConsolidationRunMetric {
                project_id: &group.project_id,
                note_type: &group.note_type,
                status: "noop",
                scanned_note_count: group.note_count,
                candidate_cluster_count: clusters.len() as i64,
                consolidated_cluster_count: qualifying_clusters.len() as i64,
                consolidated_note_count: 0,
                source_note_count: qualifying_clusters
                    .iter()
                    .map(|cluster| cluster.note_ids.len() as i64)
                    .sum(),
                started_at: &now,
                completed_at: Some(&now),
                error_message: None,
            })
            .await?;

            Ok(())
        })
    }
}

pub(super) async fn run_note_consolidation(
    db: &Database,
    consolidation_runner: &Arc<dyn ConsolidationRunner>,
) {
    let repo = NoteConsolidationRepository::new(db.clone());
    let groups = match repo.list_db_note_groups().await {
        Ok(groups) => groups,
        Err(error) => {
            tracing::warn!(error = %error, "CoordinatorActor: failed to list DB note groups for consolidation");
            return;
        }
    };

    for group in groups {
        if let Err(error) = consolidation_runner.run_for_group(group.clone()).await {
            tracing::warn!(
                project_id = %group.project_id,
                note_type = %group.note_type,
                error = %error,
                "CoordinatorActor: failed to run DB note consolidation"
            );
        }
    }
}
