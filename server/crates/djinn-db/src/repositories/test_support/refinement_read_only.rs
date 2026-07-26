use crate::database::Database;

use super::RefinementRunAuditForTest;

/// One ordered lifecycle/audit row retained for a read-only refinement-run assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementLifecycleAuditRowForTest {
    pub id: String,
    pub event_kind: String,
    pub refinement_stop_tag: Option<String>,
    pub refinement_stop_context: Option<serde_json::Value>,
}

/// One durable dispatch-intent row retained for a read-only assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementDispatchIntentRowForTest {
    pub id: String,
    pub state: String,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<String>,
    pub claim_expires_at: Option<String>,
    pub task_id: Option<String>,
    pub next_intent_id: Option<String>,
    pub terminal_at: Option<String>,
}

/// Complete durable state used to prove a refinement observation is read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementRunReadOnlySnapshotForTest {
    pub run: RefinementRunAuditForTest,
    pub heartbeat_at: String,
    pub lifecycle_rows: Vec<RefinementLifecycleAuditRowForTest>,
    pub dispatch_intents: Vec<RefinementDispatchIntentRowForTest>,
    pub durable_typed_phantom_reap_count: i64,
}

/// Read every durable value covered by the repeated refinement-read invariant.
///
/// Lifecycle rows are ordered by immutable id, enabling direct comparison of
/// snapshots without hiding additions or modifications.
pub async fn refinement_run_read_only_snapshot_for_test(
    db: &Database,
    proposal_id: &str,
    run_id: &str,
) -> RefinementRunReadOnlySnapshotForTest {
    db.ensure_initialized().await.unwrap();
    let row: (
        i32,
        String,
        Option<String>,
        Option<String>,
        Option<serde_json::Value>,
        String,
    ) = sqlx::query_as(
        "SELECT generation, state, park_kind, stop_tag, stop_context, heartbeat_at \
         FROM refinement_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_one(db.pool())
    .await
    .expect("failed to read refinement run read-only snapshot");
    let lifecycle_rows: Vec<(String, String, Option<String>, Option<serde_json::Value>)> =
        sqlx::query_as(
            "SELECT id, event_kind, refinement_stop_tag, refinement_stop_context \
         FROM proposal_revisions WHERE proposal_id = $1 AND refinement_run_id = $2 \
         ORDER BY id",
        )
        .bind(proposal_id)
        .bind(run_id)
        .fetch_all(db.pool())
        .await
        .expect("failed to read refinement lifecycle rows");
    let dispatch_intents: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, state, claimed_by, claimed_at, claim_expires_at, task_id, next_intent_id, terminal_at \
         FROM refinement_dispatch_intents WHERE run_id = $1 ORDER BY round, id",
    )
    .bind(run_id)
    .fetch_all(db.pool())
    .await
    .expect("failed to read refinement dispatch intents");
    let typed_reap_count = sqlx::query_scalar(
        "SELECT COUNT(*) FROM refinement_runs \
         WHERE id = $1 AND stop_tag = 'reaped_phantom'",
    )
    .bind(run_id)
    .fetch_one(db.pool())
    .await
    .expect("failed to count typed refinement reaps");
    let durable_typed_phantom_reap_count = sqlx::query_scalar(
        "SELECT COUNT(*) FROM proposal_revisions \
         WHERE proposal_id = $1 AND refinement_stop_tag = 'reaped_phantom'",
    )
    .bind(proposal_id)
    .fetch_one(db.pool())
    .await
    .expect("failed to count durable typed phantom reaps");

    RefinementRunReadOnlySnapshotForTest {
        run: RefinementRunAuditForTest {
            generation: row.0,
            state: row.1,
            park_kind: row.2,
            stop_tag: row.3,
            stop_context: row.4,
            typed_reap_count,
        },
        heartbeat_at: row.5,
        lifecycle_rows: lifecycle_rows
            .into_iter()
            .map(
                |(id, event_kind, refinement_stop_tag, refinement_stop_context)| {
                    RefinementLifecycleAuditRowForTest {
                        id,
                        event_kind,
                        refinement_stop_tag,
                        refinement_stop_context,
                    }
                },
            )
            .collect(),
        dispatch_intents: dispatch_intents
            .into_iter()
            .map(
                |(
                    id,
                    state,
                    claimed_by,
                    claimed_at,
                    claim_expires_at,
                    task_id,
                    next_intent_id,
                    terminal_at,
                )| {
                    RefinementDispatchIntentRowForTest {
                        id,
                        state,
                        claimed_by,
                        claimed_at,
                        claim_expires_at,
                        task_id,
                        next_intent_id,
                        terminal_at,
                    }
                },
            )
            .collect(),
        durable_typed_phantom_reap_count,
    }
}
