//! Durable terminal-report acceptance is the only completion-counter boundary.

use djinn_agent::direct_services::DirectServices;
use djinn_core::models::TaskRunStatus;
use djinn_db::repositories::task_attempt::{CreateTaskAttemptParams, TaskAttemptRepository};
use djinn_supervisor::SupervisorServices;
use djinn_supervisor::services::SerializableCreateTaskRunParams;
use tokio_util::sync::CancellationToken;

const METRIC: &str = "djinn_worker_completions_submitted_total";

fn rendered_counter(rendered: &str) -> f64 {
    rendered
        .lines()
        .find_map(|line| {
            line.strip_prefix(METRIC)
                .and_then(|suffix| suffix.strip_prefix(' '))
                .and_then(|value| value.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}

async fn create_running_run(
    services: &DirectServices,
    db: &djinn_db::Database,
    project_id: &str,
    task_id: &str,
) -> String {
    let attempt = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt,
            task_id,
            role: "worker",
            dispatch_key: &attempt,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create pending worker attempt");
    let run = uuid::Uuid::now_v7().to_string();
    services
        .create_task_run(SerializableCreateTaskRunParams {
            id: run.clone(),
            task_attempt_id: Some(attempt),
            project_id: project_id.to_owned(),
            task_id: task_id.to_owned(),
            trigger_type: "new_task".to_owned(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("persist running task run");
    run
}

#[tokio::test]
async fn completion_counter_moves_only_after_new_durable_terminal_acceptance() {
    let db = djinn_agent::test_helpers::create_test_db();
    let project = djinn_agent::test_helpers::create_test_project(&db).await;
    let epic = djinn_agent::test_helpers::create_test_epic(&db, &project.id).await;
    let task = djinn_agent::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let services = DirectServices::new(
        djinn_agent::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
        CancellationToken::new(),
    );

    // Receipt alone has no persistence operation and therefore cannot move it.
    let baseline = rendered_counter(&djinn_telemetry::render().expect("render metrics"));

    let nonterminal = create_running_run(&services, &db, &project.id, &task.id).await;
    assert!(
        services
            .update_task_run_status(nonterminal, TaskRunStatus::Running)
            .await
            .is_err(),
        "nonterminal status is validation-rejected at the terminal-report boundary"
    );
    assert_eq!(
        rendered_counter(&djinn_telemetry::render().expect("render metrics")),
        baseline,
        "receipt and validation failure must not increment the completion counter"
    );

    let accepted = create_running_run(&services, &db, &project.id, &task.id).await;
    services
        .update_task_run_status(accepted.clone(), TaskRunStatus::Completed)
        .await
        .expect("new terminal report durably accepted");
    assert_eq!(
        rendered_counter(&djinn_telemetry::render().expect("render metrics")),
        baseline + 1.0,
        "exactly one new durable terminal acceptance increments the counter"
    );

    assert!(
        services
            .update_task_run_status(accepted, TaskRunStatus::Completed)
            .await
            .is_err(),
        "duplicate terminal report is rejected"
    );
    assert!(
        services
            .update_task_run_status(uuid::Uuid::now_v7().to_string(), TaskRunStatus::Completed)
            .await
            .is_err(),
        "unknown terminal report is rejected"
    );
    assert_eq!(
        rendered_counter(&djinn_telemetry::render().expect("render metrics")),
        baseline + 1.0,
        "duplicate and rejected terminal reports must not increment the counter"
    );

    let persistence_failure = create_running_run(&services, &db, &project.id, &task.id).await;
    db.pool().close().await;
    assert!(
        services
            .update_task_run_status(persistence_failure, TaskRunStatus::Completed)
            .await
            .is_err(),
        "database persistence failure is returned to the worker"
    );
    assert_eq!(
        rendered_counter(&djinn_telemetry::render().expect("render metrics")),
        baseline + 1.0,
        "failed persistence must not increment the completion counter"
    );
}
