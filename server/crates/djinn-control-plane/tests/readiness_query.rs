//! Real-Postgres service tests for owner-authorized readiness queries.

use djinn_control_plane::readiness_query::{
    ReadinessProjectQuery, ReadinessQueryError, ReadinessQueryService, ReadinessRunQuery,
};
use djinn_core::events::EventBus;
use djinn_db::{
    CreateReadinessAreaAttempt, CreateReadinessCompositionArea, CreateReadinessRun, Database,
    ProjectRepository, ReadinessRepository, UserRepository,
};

#[tokio::test]
async fn owner_reads_active_precedence_and_complete_repository_detail_projection() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id, outsider_id) = seed_project_and_users(&db, "query-owner").await;
    let repository = ReadinessRepository::new(db.clone());

    let latest_terminal = repository
        .create_run(run_input(&project_id, "terminal"))
        .await
        .expect("terminal run");
    repository
        .fail_identification(&latest_terminal.id, "fixture terminal state")
        .await
        .expect("terminalize fixture");
    let active = repository
        .create_run(run_input(&project_id, "active"))
        .await
        .expect("active run");
    let area = repository
        .create_area(CreateReadinessCompositionArea {
            run_id: active.id.clone(),
            area_key: "frontend".into(),
            composition: serde_json::json!({"languages":["TypeScript"]}),
            path_scopes: serde_json::json!(["web/"]),
        })
        .await
        .expect("area");
    let attempt = repository
        .create_attempt(CreateReadinessAreaAttempt {
            run_id: active.id.clone(),
            area_id: area.id.clone(),
            attempt_number: 1,
            correlation_key: "attempt-key".into(),
        })
        .await
        .expect("attempt");
    djinn_db::test_support::seed_readiness_detail_projection_for_test(
        &db,
        &active.id,
        &area.id,
        &attempt.id,
    )
    .await;

    let service = ReadinessQueryService::new(db.clone());
    let summary = service
        .active_or_latest(project_query(&project_id, &owner_id))
        .await
        .expect("owner reads active run")
        .expect("active run exists");
    assert_eq!(
        summary.id, active.id,
        "active run precedes newer terminal fallback"
    );
    assert_eq!(summary.skill_version, "1.0.0");

    let detail = service
        .run_detail(run_query(&project_id, &active.id, &owner_id))
        .await
        .expect("owner reads complete detail");
    assert_eq!(detail.run.id, active.id);
    assert_eq!(detail.areas.len(), 1);
    assert_eq!(detail.areas[0].area_key, "frontend");
    assert!(detail.areas[0].attempts[0].is_current);
    assert_eq!(detail.areas[0].accepted_findings[0].guardrail_key, "auth");
    assert_eq!(
        detail.areas[0].accepted_outputs[0].result["warnings"][0],
        "preserved"
    );
    assert_eq!(detail.area_scores[0].score, 0.75);
    assert_eq!(detail.project_score.expect("score").band, "ready");
    assert_eq!(detail.suggestions[0].dedupe_key, "auth-remediation");
    assert_eq!(detail.events[0].event_kind, "fixture_completed");

    let forbidden = service
        .active_or_latest(project_query(&project_id, &outsider_id))
        .await
        .expect_err("non-owner cannot read runs");
    assert!(matches!(
        forbidden,
        ReadinessQueryError::UnauthorizedOwner { .. }
    ));
}

#[tokio::test]
async fn query_failures_are_explicit_without_cross_project_detail_leakage() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (left_project, owner_id, _) = seed_project_and_users(&db, "query-left").await;
    let right = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("query-right", "query-owner", "right")
        .await
        .expect("right project");
    let right_run = ReadinessRepository::new(db.clone())
        .create_run(run_input(&right.id, "right-run"))
        .await
        .expect("right run");
    let service = ReadinessQueryService::new(db);

    assert!(matches!(
        service
            .active_or_latest(project_query("missing-project", &owner_id))
            .await,
        Err(ReadinessQueryError::ProjectNotFound { .. })
    ));
    assert!(matches!(
        service
            .active_or_latest(project_query(&left_project, "missing-user"))
            .await,
        Err(ReadinessQueryError::AuthenticatedOwnerNotFound { .. })
    ));
    assert!(matches!(
        service
            .run_detail(run_query(&left_project, "missing-run", &owner_id))
            .await,
        Err(ReadinessQueryError::RunNotFound { .. })
    ));
    let mismatch = service
        .run_detail(run_query(&left_project, &right_run.id, &owner_id))
        .await
        .expect_err("run cannot be read through another project");
    assert!(matches!(
        mismatch,
        ReadinessQueryError::RunProjectMismatch { .. }
    ));
}

fn run_input(project_id: &str, key: &str) -> CreateReadinessRun {
    CreateReadinessRun {
        project_id: project_id.into(),
        idempotency_key: key.into(),
        repository_snapshot: "fixture-snapshot".into(),
        skill_name: "agent-readiness-guardrails".into(),
        skill_version: "1.0.0".into(),
    }
}

fn project_query(project_id: &str, owner_id: &str) -> ReadinessProjectQuery {
    ReadinessProjectQuery {
        project_id: project_id.into(),
        authenticated_owner_id: owner_id.into(),
    }
}

fn run_query(project_id: &str, run_id: &str, owner_id: &str) -> ReadinessRunQuery {
    ReadinessRunQuery {
        project_id: project_id.into(),
        run_id: run_id.into(),
        authenticated_owner_id: owner_id.into(),
    }
}

async fn seed_project_and_users(db: &Database, name: &str) -> (String, String, String) {
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create(name, "query-owner", name)
        .await
        .expect("project");
    let users = UserRepository::new(db.clone());
    let owner = users
        .upsert_from_github(602_001, "query-owner", None, None)
        .await
        .expect("owner");
    let outsider = users
        .upsert_from_github(602_002, "query-outsider", None, None)
        .await
        .expect("outsider");
    (project.id, owner.id, outsider.id)
}
