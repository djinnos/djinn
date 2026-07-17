//! Regression coverage for arbiter decision consumption.
//!
//! An approve decision must mark the arbitration row consumed. A row left
//! unconsumed after approve wedges the task if it re-enters the second-strike
//! path (e.g. a merge-conflict reopen): the coordinator treats the stale row
//! as "arbiter already in flight" every tick and dispatches nothing until the
//! arbitration deadline (incident lre2, 2026-07-16).
//!
//! Lives in its own integration-test file (rather than
//! `arbiter_park_transaction.rs`) to stay under the server size guard.

use std::sync::Arc;

use djinn_agent::supervisor::{SupervisorServices, services_for_agent_context};
use djinn_agent::test_helpers;
use djinn_db::repositories::task_arbitration::{
    CreateArbitrationParams, TaskArbitrationRepository,
};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbiter_approve_marks_arbitration_row_consumed() {
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let arb_repo = TaskArbitrationRepository::new(db.clone());
    let failing_jobs = serde_json::json!([]);
    let ex = serde_json::json!([]);
    arb_repo
        .try_create(CreateArbitrationParams {
            task_id: &task.id,
            hold_cycle: 0,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &failing_jobs,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &ex,
        })
        .await
        .expect("create arbitration row");

    let ctx = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let services: Arc<dyn SupervisorServices> =
        services_for_agent_context(ctx, CancellationToken::new());

    services
        .record_arbiter_decision(
            task.id.clone(),
            "approve".into(),
            r#"{"summary": "looks good"}"#.into(),
        )
        .await
        .expect("record_arbiter_decision must succeed");

    let record = arb_repo
        .get_by_task_and_cycle(&task.id, 0)
        .await
        .expect("read arbitration row")
        .expect("arbitration row exists");
    assert_eq!(
        record.state, "consumed",
        "approve must mark the arbitration row consumed"
    );
    assert!(
        record.consumed_at.is_some(),
        "consumed_at must be stamped on approve"
    );
    assert_eq!(
        record
            .directive
            .as_ref()
            .and_then(|d| d["decision"].as_str()),
        Some("approve"),
        "decision must be persisted on the row before consumption"
    );
}
