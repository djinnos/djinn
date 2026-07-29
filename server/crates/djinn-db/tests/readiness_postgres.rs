//! Real-Postgres contract tests for readiness persistence migrations.
// djinn:allow-oversize -- one sequential migration/transaction contract suite.
//!
//! These tests deliberately create a fresh database, replay the migrations
//! before 155, and execute the readiness migrations directly. The constraints
//! below are therefore exercised by Postgres rather than an in-memory substitute.

use std::path::{Path, PathBuf};

use djinn_core::models::TaskExecutionContext;
use djinn_db::{
    Database, UserRepository,
    repositories::readiness::{
        CreateReadinessAreaAttempt, CreateReadinessCompositionArea, CreateReadinessRun,
        MaterializeReadinessKickoff, NewReadinessEvent, NewReadinessFinding,
        ReadinessAreaResultCallback, ReadinessCallbackOutcome, ReadinessIdentificationOutput,
        ReadinessIdentifiedArea, ReadinessRepository, RetryReadinessAreaAttempt,
    },
};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 155;
const MIGRATION_FILE: &str = "155_readiness_persistence.sql";
const FINDING_CONFIDENCE_MIGRATION_FILE: &str = "157_readiness_finding_confidence.sql";
const RESULT_OUTPUTS_MIGRATION_FILE: &str = "159_readiness_result_outputs.sql";
const CURRENT_ATTEMPT_MIGRATION_FILE: &str = "160_readiness_current_attempt.sql";
const DESIGNATED_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000155";

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

#[tokio::test]
async fn materialized_kickoff_redelivery_and_racing_keys_converge_on_one_task() {
    // Database::ephemeral is a template-cloned, real Postgres database.
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    let redelivery_project_id = "materialized-readiness-redelivery";
    let racing_project_id = "materialized-readiness-race";
    // Project names are globally unique, so each fixture project needs its own
    // name even though they share this test database.
    djinn_db::test_support::seed_project(&db, redelivery_project_id, "readiness-redelivery").await;
    djinn_db::test_support::seed_project(&db, racing_project_id, "readiness-race").await;
    let creator_user_id = seed_user(&db, 155_001, "readiness-kickoff-creator").await;
    let repo = ReadinessRepository::new(db.clone());

    let first = repo
        .materialize_kickoff(kickoff_input(
            redelivery_project_id,
            &creator_user_id,
            "same-key",
        ))
        .await
        .expect("materialize first kickoff");
    let redelivery = repo
        .materialize_kickoff(kickoff_input(
            redelivery_project_id,
            &creator_user_id,
            "same-key",
        ))
        .await
        .expect("materialize same-key redelivery");
    assert_eq!(redelivery.run.id, first.run.id);
    assert_eq!(
        redelivery.identification_task.id,
        first.identification_task.id
    );
    assert_eq!(kickoff_counts(&db, redelivery_project_id).await, (1, 1));

    let left_repo = ReadinessRepository::new(db.clone());
    let right_repo = ReadinessRepository::new(db.clone());
    let (left, right) = tokio::join!(
        left_repo.materialize_kickoff(kickoff_input(
            racing_project_id,
            &creator_user_id,
            "race-left",
        )),
        right_repo.materialize_kickoff(kickoff_input(
            racing_project_id,
            &creator_user_id,
            "race-right",
        )),
    );
    let left = left.expect("materialize first racing key");
    let right = right.expect("materialize second racing key");
    assert_eq!(left.run.id, right.run.id);
    assert_eq!(left.identification_task.id, right.identification_task.id);
    assert_eq!(
        kickoff_counts(&db, racing_project_id).await,
        (1, 1),
        "racing keys leave one run and identification task"
    );
}

#[tokio::test]
async fn materialized_kickoff_validation_failures_leave_no_run_or_task() {
    // Database::ephemeral is a template-cloned, real Postgres database.
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    let project_id = "materialized-readiness-validation";
    djinn_db::test_support::seed_project(&db, project_id, "readiness").await;
    let creator_user_id = seed_user(&db, 155_002, "readiness-validation-creator").await;
    let repo = ReadinessRepository::new(db.clone());

    for field in [
        "idempotency_key",
        "repository_snapshot",
        "skill_name",
        "skill_version",
    ] {
        let mut input = kickoff_input(project_id, &creator_user_id, "validation-key");
        match field {
            "idempotency_key" => input.idempotency_key = " \t ".into(),
            "repository_snapshot" => input.repository_snapshot = " \t ".into(),
            "skill_name" => input.skill_name = " \t ".into(),
            "skill_version" => input.skill_version = " \t ".into(),
            _ => unreachable!("known fixture field"),
        }
        repo.materialize_kickoff(input)
            .await
            .expect_err("blank validated kickoff field must fail");
        assert_eq!(
            kickoff_counts(&db, project_id).await,
            (0, 0),
            "blank {field} must not persist readiness state"
        );
    }

    repo.materialize_kickoff(kickoff_input(
        project_id,
        "nonexistent-readiness-creator",
        "invalid-creator-key",
    ))
    .await
    .expect_err("invalid explicit creator identity must fail");
    assert_eq!(
        kickoff_counts(&db, project_id).await,
        (0, 0),
        "invalid creator must not persist readiness state"
    );
}

#[tokio::test]
async fn identification_failure_paths_leave_no_area_fanout() {
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    let project = "readiness-identification-failure";
    djinn_db::test_support::seed_project(&db, project, "readiness-identification-failure").await;
    let creator = seed_user(&db, 155_003, "readiness-identification-creator").await;
    let repo = ReadinessRepository::new(db.clone());

    let invalid = repo
        .materialize_kickoff(kickoff_input(project, &creator, "invalid"))
        .await
        .expect("materialize invalid fixture");
    repo.complete_identification(
        &invalid.run.id,
        &creator,
        ReadinessIdentificationOutput { areas: vec![] },
    )
    .await
    .expect_err("zero areas fails identification");
    assert_failed_without_fanout(
        &db,
        &invalid.run.id,
        "identification output must contain at least one area",
    )
    .await;

    let explicit = repo
        .materialize_kickoff(kickoff_input(project, &creator, "explicit"))
        .await
        .expect("materialize explicit fixture");
    repo.fail_identification(&explicit.run.id, "architect failure")
        .await
        .expect("explicit failure terminalizes");
    assert_failed_without_fanout(&db, &explicit.run.id, "architect failure").await;
}

#[tokio::test]
async fn identification_completion_persists_full_area_task_payloads_and_context() {
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    let project = "readiness-identification-completion";
    djinn_db::test_support::seed_project(&db, project, "readiness-identification-completion").await;
    let creator = seed_user(&db, 155_004, "readiness-completion-creator").await;
    let repo = ReadinessRepository::new(db.clone());
    let kickoff = repo
        .materialize_kickoff(kickoff_input(project, &creator, "complete"))
        .await
        .expect("materialize completion fixture");
    let output = identification_output();
    let fanout = repo
        .complete_identification(&kickoff.run.id, &creator, output.clone())
        .await
        .expect("complete valid identification");
    assert_eq!(fanout_counts(&db, &kickoff.run.id).await, (2, 2, 2));
    let run: (String, Option<i32>) =
        sqlx::query_as("SELECT status,expected_area_count FROM readiness_runs WHERE id=$1")
            .bind(&kickoff.run.id)
            .fetch_one(db.pool())
            .await
            .expect("load run");
    assert_eq!(run, ("analyzing".into(), Some(2)));
    let completion_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM readiness_run_events WHERE run_id=$1 AND event_kind='identification_completed'",
    )
    .bind(&kickoff.run.id)
    .fetch_one(db.pool())
    .await
    .expect("count identification completion event");
    assert_eq!(completion_events, 1);

    let expected_context =
        TaskExecutionContext::readiness_guardrail_analysis("agent-readiness-guardrails", "1.0.0")
            .expect("context");
    let mut correlations = std::collections::HashSet::new();
    for item in &fanout {
        let task: PersistedAreaTask = sqlx::query_as("SELECT project_id,owner,description,agent_type,execution_context FROM tasks WHERE id=$1")
            .bind(&item.task.id).fetch_one(db.pool()).await.expect("load persisted area task");
        let description: serde_json::Value =
            serde_json::from_str(&task.description).expect("task description JSON");
        let identified = output
            .areas
            .iter()
            .find(|area| area.area_key == item.area.area_key)
            .expect("matching area");
        assert_eq!(task.project_id, project);
        assert_eq!(task.owner, creator);
        assert_eq!(task.agent_type.as_deref(), Some("architect"));
        assert_eq!(task.execution_context, Some(expected_context.clone()));
        assert_eq!(description["kind"], "readiness_area_analysis");
        assert_eq!(description["run_id"], kickoff.run.id);
        assert_eq!(description["area_id"], item.area.id);
        assert_eq!(description["area_key"], item.area.area_key);
        assert_eq!(description["attempt_id"], item.attempt.id);
        assert_eq!(description["attempt_number"], 1);
        assert_eq!(description["correlation_key"], item.attempt.correlation_key);
        assert_eq!(description["project_id"], project);
        assert_eq!(description["owner"], creator);
        assert_eq!(
            description["repository_snapshot"],
            "sha256:readiness-fixture"
        );
        assert_eq!(description["skill_name"], "agent-readiness-guardrails");
        assert_eq!(description["skill_version"], "1.0.0");
        let composition = serde_json::json!({
            "languages": &identified.languages,
            "roles": &identified.roles,
            "frameworks": &identified.frameworks,
            "key_libraries": &identified.key_libraries,
            "confidence": identified.confidence,
            "evidence": &identified.evidence,
        });
        assert_eq!(
            description["path_scopes"],
            serde_json::json!(&identified.path_scopes)
        );
        assert_eq!(description["composition"], composition);
        assert_eq!(item.area.composition, composition);
        assert_eq!(
            item.area.path_scopes,
            serde_json::json!(&identified.path_scopes)
        );
        assert_eq!(item.attempt.attempt_number, 1);
        assert!(correlations.insert(item.attempt.correlation_key.clone()));
    }
}

#[tokio::test]
async fn identification_fanout_database_failure_rolls_back_every_artifact() {
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    let project = "readiness-identification-rollback";
    djinn_db::test_support::seed_project(&db, project, "readiness-identification-rollback").await;
    let creator = seed_user(&db, 155_005, "readiness-rollback-creator").await;
    let repo = ReadinessRepository::new(db.clone());
    let kickoff = repo
        .materialize_kickoff(kickoff_input(project, &creator, "rollback"))
        .await
        .expect("materialize rollback fixture");
    sqlx::query("CREATE FUNCTION readiness_test_abort_second_area() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF (SELECT count(*) FROM readiness_composition_areas WHERE run_id=NEW.run_id) >= 1 THEN RAISE EXCEPTION 'injected readiness fanout failure'; END IF; RETURN NEW; END; $$")
        .execute(db.pool()).await.expect("install mid-fanout failure function");
    sqlx::query("CREATE TRIGGER readiness_test_abort_second_area BEFORE INSERT ON readiness_composition_areas FOR EACH ROW EXECUTE FUNCTION readiness_test_abort_second_area()")
        .execute(db.pool()).await.expect("install mid-fanout failure trigger");
    let error = repo
        .complete_identification(&kickoff.run.id, &creator, identification_output())
        .await
        .expect_err("second area aborts transaction");
    assert!(
        error
            .to_string()
            .contains("injected readiness fanout failure")
    );
    assert_eq!(fanout_counts(&db, &kickoff.run.id).await, (0, 0, 0));
    let run: (String, Option<i32>) =
        sqlx::query_as("SELECT status,expected_area_count FROM readiness_runs WHERE id=$1")
            .bind(&kickoff.run.id)
            .fetch_one(db.pool())
            .await
            .expect("load rollback run");
    assert_eq!(run, ("identifying".into(), None));
}

#[tokio::test]
async fn area_callback_redelivery_conflict_and_retry_are_postgres_transactional() {
    let db = Database::ephemeral().await.expect("postgres");
    let project = "readiness-area-callback";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let creator = seed_user(&db, 155_006, "readiness-area-callback").await;
    let repo = ReadinessRepository::new(db.clone());
    let kickoff = repo
        .materialize_kickoff(kickoff_input(project, &creator, "callback"))
        .await
        .expect("kickoff");
    let fanout = repo
        .complete_identification(&kickoff.run.id, &creator, identification_output())
        .await
        .expect("fanout");
    let first = &fanout[0];
    let callback = ReadinessAreaResultCallback {
        run_id: kickoff.run.id.clone(),
        area_id: first.area.id.clone(),
        attempt_id: first.attempt.id.clone(),
        correlation_key: first.attempt.correlation_key.clone(),
        task_id: first.task.id.clone(),
        status: "succeeded".into(),
        result: callback_result(),
    };
    assert_eq!(
        repo.ingest_area_result(callback.clone())
            .await
            .expect("accepted"),
        ReadinessCallbackOutcome::Accepted
    );
    let accepted_outputs: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT result FROM readiness_area_result_outputs WHERE attempt_id=$1")
            .bind(&first.attempt.id)
            .fetch_all(db.pool())
            .await
            .expect("load accepted callback output");
    assert_eq!(
        accepted_outputs,
        vec![callback.result.clone()],
        "the current successful winner persists its complete callback document"
    );
    assert_eq!(
        repo.ingest_area_result(callback.clone())
            .await
            .expect("same digest"),
        ReadinessCallbackOutcome::Redelivered
    );
    let mut changed = callback.clone();
    changed.result["findings"][0]["guardrail_key"] = serde_json::json!("changed");
    assert_eq!(
        repo.ingest_area_result(changed).await.expect("conflict"),
        ReadinessCallbackOutcome::Conflict
    );
    let output_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM readiness_area_result_outputs WHERE attempt_id=$1",
    )
    .bind(&first.attempt.id)
    .fetch_one(db.pool())
    .await
    .expect("count accepted winner outputs");
    assert_eq!(
        output_count, 1,
        "redeliveries and conflicting duplicates do not add another output row"
    );
    let findings: i64 =
        sqlx::query_scalar("SELECT count(*) FROM readiness_guardrail_findings WHERE attempt_id=$1")
            .bind(&first.attempt.id)
            .fetch_one(db.pool())
            .await
            .expect("findings");
    assert_eq!(
        findings, 1,
        "same digest redelivery never duplicates findings"
    );
    let finding: (f64, serde_json::Value) = sqlx::query_as(
        "SELECT confidence,evidence FROM readiness_guardrail_findings WHERE attempt_id=$1",
    )
    .bind(&first.attempt.id)
    .fetch_one(db.pool())
    .await
    .expect("complete accepted finding");
    assert_eq!(finding, (0.9, serde_json::json!([{"path":"src/auth.rs"}])));

    let second = &fanout[1];
    let failed = ReadinessAreaResultCallback {
        run_id: kickoff.run.id.clone(),
        area_id: second.area.id.clone(),
        attempt_id: second.attempt.id.clone(),
        correlation_key: second.attempt.correlation_key.clone(),
        task_id: second.task.id.clone(),
        status: "failed".into(),
        result: serde_json::json!({}),
    };
    assert_eq!(
        repo.ingest_area_result(failed).await.expect("failed"),
        ReadinessCallbackOutcome::Accepted
    );
    let failed_output_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM readiness_area_result_outputs WHERE attempt_id=$1",
    )
    .bind(&second.attempt.id)
    .fetch_one(db.pool())
    .await
    .expect("count failed callback outputs");
    assert_eq!(
        failed_output_count, 0,
        "terminal failed callbacks do not persist accepted output documents"
    );
    let retry = repo
        .retry_area_attempt(RetryReadinessAreaAttempt {
            run_id: kickoff.run.id.clone(),
            area_id: second.area.id.clone(),
            creator_user_id: creator,
        })
        .await
        .expect("retry");
    assert_eq!(retry.attempt.attempt_number, 2);
    assert_ne!(
        retry.attempt.correlation_key,
        second.attempt.correlation_key
    );
    let late = ReadinessAreaResultCallback {
        run_id: kickoff.run.id.clone(),
        area_id: second.area.id.clone(),
        attempt_id: second.attempt.id.clone(),
        correlation_key: second.attempt.correlation_key.clone(),
        task_id: second.task.id.clone(),
        status: "succeeded".into(),
        result: callback_result(),
    };
    assert_eq!(
        repo.ingest_area_result(late).await.expect("late"),
        ReadinessCallbackOutcome::Ignored
    );
    let late_output_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM readiness_area_result_outputs WHERE attempt_id IN ($1,$2)",
    )
    .bind(&second.attempt.id)
    .bind(&retry.attempt.id)
    .fetch_one(db.pool())
    .await
    .expect("count failed and superseded callback outputs");
    assert_eq!(
        late_output_count, 0,
        "late non-current callbacks do not persist output documents"
    );
}

fn callback_result() -> serde_json::Value {
    serde_json::json!({"findings":[{"guardrail_key":"auth","status":"covered","severity":"high","confidence":0.9,"evidence":[{"path":"src/auth.rs"}]}],"unsupported":[{"reason":"fixture"}],"warnings":[{"warning":"fixture"}],"remediation_suggestions":[{"dedupe_key":"auth","action":"fix"}]})
}

#[tokio::test]
async fn area_callbacks_deduplicate_run_level_suggestions_without_rolling_back_success() {
    let db = Database::ephemeral().await.expect("postgres");
    let project = "readiness-area-suggestion-dedup";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let creator = seed_user(&db, 155_008, "readiness-area-suggestion-deduplication").await;
    let repo = ReadinessRepository::new(db.clone());
    let kickoff = repo
        .materialize_kickoff(kickoff_input(project, &creator, "suggestion-deduplication"))
        .await
        .expect("kickoff");
    let fanout = repo
        .complete_identification(&kickoff.run.id, &creator, identification_output())
        .await
        .expect("two-area fanout");
    assert_eq!(fanout.len(), 2);

    for item in &fanout {
        let callback = ReadinessAreaResultCallback {
            run_id: kickoff.run.id.clone(),
            area_id: item.area.id.clone(),
            attempt_id: item.attempt.id.clone(),
            correlation_key: item.attempt.correlation_key.clone(),
            task_id: item.task.id.clone(),
            status: "succeeded".into(),
            result: callback_result(),
        };
        assert_eq!(
            repo.ingest_area_result(callback)
                .await
                .expect("cross-area suggestion collision is idempotent"),
            ReadinessCallbackOutcome::Accepted
        );
    }

    let attempts: Vec<(String, String)> = sqlx::query_as(
        "SELECT id,status FROM readiness_area_attempts WHERE run_id=$1 ORDER BY area_id",
    )
    .bind(&kickoff.run.id)
    .fetch_all(db.pool())
    .await
    .expect("load terminal attempts");
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|(_, status)| status == "succeeded"));
    for (attempt_id, _) in &attempts {
        let findings: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM readiness_guardrail_findings WHERE attempt_id=$1",
        )
        .bind(attempt_id)
        .fetch_one(db.pool())
        .await
        .expect("load accepted findings");
        assert_eq!(
            findings, 1,
            "each area's complete finding set remains visible"
        );
    }
    let suggestions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM readiness_remediation_suggestions WHERE run_id=$1 AND dedupe_key='auth'",
    )
    .bind(&kickoff.run.id)
    .fetch_one(db.pool())
    .await
    .expect("load deduplicated suggestion");
    assert_eq!(suggestions, 1);
}

#[tokio::test]
async fn aggregators_are_fenced_and_persist_one_terminal_score_set() {
    let db = Database::ephemeral().await.expect("postgres");
    let project = "readiness-aggregation-race";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let creator = seed_user(&db, 155_009, "readiness-aggregation-race").await;
    let repo = ReadinessRepository::new(db.clone());
    let kickoff = repo
        .materialize_kickoff(kickoff_input(project, &creator, "completed"))
        .await
        .expect("kickoff");
    let fanout = repo
        .complete_identification(&kickoff.run.id, &creator, identification_output())
        .await
        .expect("fanout");
    for (index, item) in fanout.iter().enumerate() {
        let mut result = callback_result();
        result["findings"][0]["status"] =
            serde_json::json!(if index == 0 { "missing" } else { "covered" });
        result["remediation_suggestions"][0]["guardrail_ids"] =
            serde_json::json!([format!("guardrail-{index}")]);
        repo.ingest_area_result(ReadinessAreaResultCallback {
            run_id: kickoff.run.id.clone(),
            area_id: item.area.id.clone(),
            attempt_id: item.attempt.id.clone(),
            correlation_key: item.attempt.correlation_key.clone(),
            task_id: item.task.id.clone(),
            status: "succeeded".into(),
            result,
        })
        .await
        .expect("accepted callback");
    }
    let left = ReadinessRepository::new(db.clone());
    let right = ReadinessRepository::new(db.clone());
    let (first, second) = tokio::join!(
        left.aggregate_run(&kickoff.run.id, "worker-a"),
        right.aggregate_run(&kickoff.run.id, "worker-b")
    );
    assert_eq!(first.expect("first aggregate").status, "completed");
    assert_eq!(second.expect("second aggregate").status, "completed");
    let persisted_status: String =
        sqlx::query_scalar("SELECT status FROM readiness_runs WHERE id=$1")
            .bind(&kickoff.run.id)
            .fetch_one(db.pool())
            .await
            .expect("run status");
    assert_eq!(persisted_status, "completed");
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM readiness_area_scores WHERE run_id=$1), (SELECT count(*) FROM readiness_project_scores WHERE run_id=$1), (SELECT count(*) FROM readiness_run_events WHERE run_id=$1 AND event_kind='readiness_aggregated')").bind(&kickoff.run.id).fetch_one(db.pool()).await.expect("one aggregation set");
    assert_eq!(counts, (2, 1, 1));
    let terminal_detail = repo
        .run_detail(&kickoff.run.id)
        .await
        .expect("terminal detail")
        .expect("terminal run");
    assert_eq!(terminal_detail.run.status, "completed");
    assert_eq!(terminal_detail.area_scores.len(), 2);
    assert!(
        terminal_detail
            .area_scores
            .windows(2)
            .all(|scores| scores[0].area_id <= scores[1].area_id)
    );
    assert!(terminal_detail.project_score.is_some());
    assert_eq!(terminal_detail.suggestions.len(), 1);
    assert_eq!(
        terminal_detail
            .events
            .iter()
            .filter(|event| event.event_kind == "readiness_aggregated")
            .count(),
        1
    );
    assert_eq!(
        terminal_detail,
        repo.run_detail(&kickoff.run.id)
            .await
            .expect("repeat terminal detail")
            .expect("terminal run")
    );
    let (missing_status, missing_score): (String, f64) = sqlx::query_as("SELECT f.status,s.score FROM readiness_guardrail_findings f JOIN readiness_area_scores s ON s.area_id=f.area_id WHERE f.run_id=$1 AND f.status='missing'").bind(&kickoff.run.id).fetch_one(db.pool()).await.expect("persisted missing finding");
    assert_eq!((missing_status.as_str(), missing_score), ("missing", 0.0));
    let suggestion: serde_json::Value = sqlx::query_scalar("SELECT suggestion FROM readiness_remediation_suggestions WHERE run_id=$1 AND dedupe_key='auth'").bind(&kickoff.run.id).fetch_one(db.pool()).await.expect("merged suggestion");
    assert_eq!(
        suggestion["area_ids"].as_array().expect("area ids").len(),
        2
    );
    assert_eq!(
        suggestion["guardrail_ids"],
        serde_json::json!(["guardrail-0", "guardrail-1"])
    );

    let errors = repo
        .materialize_kickoff(kickoff_input(project, &creator, "with-errors"))
        .await
        .expect("error run");
    let error_fanout = repo
        .complete_identification(&errors.run.id, &creator, identification_output())
        .await
        .expect("error fanout");
    let succeeded = &error_fanout[0];
    repo.ingest_area_result(ReadinessAreaResultCallback {
        run_id: errors.run.id.clone(),
        area_id: succeeded.area.id.clone(),
        attempt_id: succeeded.attempt.id.clone(),
        correlation_key: succeeded.attempt.correlation_key.clone(),
        task_id: succeeded.task.id.clone(),
        status: "succeeded".into(),
        result: callback_result(),
    })
    .await
    .expect("success terminal");
    let failed = &error_fanout[1];
    repo.ingest_area_result(ReadinessAreaResultCallback {
        run_id: errors.run.id.clone(),
        area_id: failed.area.id.clone(),
        attempt_id: failed.attempt.id.clone(),
        correlation_key: failed.attempt.correlation_key.clone(),
        task_id: failed.task.id.clone(),
        status: "timed_out".into(),
        result: serde_json::json!({}),
    })
    .await
    .expect("timeout terminal");
    assert_eq!(
        repo.aggregate_run(&errors.run.id, "worker-errors")
            .await
            .expect("aggregate errors")
            .status,
        "completed_with_errors"
    );
}

#[tokio::test]
async fn area_callback_timeout_success_race_and_invalid_output_are_postgres_transactional() {
    let db = Database::ephemeral().await.expect("postgres");
    let project = "readiness-area-race";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let creator = seed_user(&db, 155_007, "readiness-area-race").await;
    let repo = ReadinessRepository::new(db.clone());
    let kickoff = repo
        .materialize_kickoff(kickoff_input(project, &creator, "race"))
        .await
        .expect("kickoff");
    let fanout = repo
        .complete_identification(&kickoff.run.id, &creator, identification_output())
        .await
        .expect("fanout");
    let race = &fanout[0];
    let success = ReadinessAreaResultCallback {
        run_id: kickoff.run.id.clone(),
        area_id: race.area.id.clone(),
        attempt_id: race.attempt.id.clone(),
        correlation_key: race.attempt.correlation_key.clone(),
        task_id: race.task.id.clone(),
        status: "succeeded".into(),
        result: callback_result(),
    };
    let timeout = ReadinessAreaResultCallback {
        run_id: kickoff.run.id.clone(),
        area_id: race.area.id.clone(),
        attempt_id: race.attempt.id.clone(),
        correlation_key: race.attempt.correlation_key.clone(),
        task_id: race.task.id.clone(),
        status: "timed_out".into(),
        result: serde_json::json!({}),
    };
    let left_repo = ReadinessRepository::new(db.clone());
    let right_repo = ReadinessRepository::new(db.clone());
    let (left, right) = tokio::join!(
        left_repo.ingest_area_result(success),
        right_repo.ingest_area_result(timeout)
    );
    let outcomes = [
        left.expect("success callback"),
        right.expect("timeout callback"),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReadinessCallbackOutcome::Accepted)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReadinessCallbackOutcome::Conflict)
            .count(),
        1
    );
    let terminal: String =
        sqlx::query_scalar("SELECT status FROM readiness_area_attempts WHERE id=$1")
            .bind(&race.attempt.id)
            .fetch_one(db.pool())
            .await
            .expect("terminal status");
    let findings: i64 =
        sqlx::query_scalar("SELECT count(*) FROM readiness_guardrail_findings WHERE attempt_id=$1")
            .bind(&race.attempt.id)
            .fetch_one(db.pool())
            .await
            .expect("findings");
    assert_eq!(
        findings,
        if terminal == "succeeded" { 1 } else { 0 },
        "only winning success inserts findings"
    );

    let invalid = &fanout[1];
    let malformed = ReadinessAreaResultCallback {
        run_id: kickoff.run.id.clone(),
        area_id: invalid.area.id.clone(),
        attempt_id: invalid.attempt.id.clone(),
        correlation_key: invalid.attempt.correlation_key.clone(),
        task_id: invalid.task.id.clone(),
        status: "succeeded".into(),
        result: serde_json::json!({"findings":[{"guardrail_key":"bad","severity":"high","confidence":0.5,"evidence":null}],"unsupported":[null],"warnings":[{"warning":null}],"remediation_suggestions":[{"dedupe_key":"bad","action":null}]}),
    };
    assert_eq!(
        repo.ingest_area_result(malformed)
            .await
            .expect("invalid terminalizes"),
        ReadinessCallbackOutcome::Accepted
    );
    let invalid_state: (String, i64, i64) = sqlx::query_as("SELECT a.status,(SELECT count(*) FROM readiness_guardrail_findings f WHERE f.attempt_id=a.id),(SELECT count(*) FROM readiness_area_result_outputs o WHERE o.attempt_id=a.id) FROM readiness_area_attempts a WHERE a.id=$1").bind(&invalid.attempt.id).fetch_one(db.pool()).await.expect("invalid state");
    assert_eq!(invalid_state, ("invalid".into(), 0, 0));
}

#[tokio::test]
async fn run_detail_is_deterministic_and_excludes_historical_accepted_output() {
    let db = Database::ephemeral().await.expect("postgres");
    let project = "readiness-detail";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let repo = ReadinessRepository::new(db.clone());
    let run = repo
        .create_run(CreateReadinessRun {
            project_id: project.into(),
            idempotency_key: "detail".into(),
            repository_snapshot: "snapshot-detail".into(),
            skill_name: "skill-detail".into(),
            skill_version: "2.0.0".into(),
        })
        .await
        .expect("run");
    for (area, key) in [("detail-front", "frontend"), ("detail-back", "backend")] {
        sqlx::query("INSERT INTO readiness_composition_areas (id,run_id,area_key,composition,path_scopes) VALUES ($1,$2,$3,'{}','[]')").bind(area).bind(&run.id).bind(key).execute(db.pool()).await.expect("area");
    }
    for (id, area, number) in [
        ("old", "detail-front", 1),
        ("current", "detail-front", 2),
        ("back-current", "detail-back", 1),
    ] {
        sqlx::query("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,$4,$5)").bind(id).bind(&run.id).bind(area).bind(number).bind(format!("key-{id}")).execute(db.pool()).await.expect("attempt");
    }
    for (area, attempt) in [("detail-front", "current"), ("detail-back", "back-current")] {
        sqlx::query("UPDATE readiness_composition_areas SET current_attempt_id=$1 WHERE id=$2")
            .bind(attempt)
            .bind(area)
            .execute(db.pool())
            .await
            .expect("current");
    }
    for (id, attempt, key) in [
        ("old-finding", "old", "old"),
        ("current-finding", "current", "current"),
    ] {
        sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,status,severity,confidence,accepted,evidence) VALUES ($1,$2,'detail-front',$3,$4,'covered','high',0.9,true,'{}')").bind(id).bind(&run.id).bind(attempt).bind(key).execute(db.pool()).await.expect("finding");
    }
    for (attempt, output) in [
        ("old", serde_json::json!({"warnings":["historical"]})),
        (
            "current",
            serde_json::json!({"warnings":["current"],"errors":[{"message":"preserved"}],"unsupported":[{"reason":"kept"}]}),
        ),
    ] {
        sqlx::query("INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ($1,'detail-front',$2,$3)").bind(&run.id).bind(attempt).bind(output).execute(db.pool()).await.expect("output");
    }
    let first = repo
        .run_detail(&run.id)
        .await
        .expect("detail")
        .expect("exists");
    let second = repo
        .run_detail(&run.id)
        .await
        .expect("detail")
        .expect("exists");
    assert_eq!(first, second);
    assert_eq!(first.run.repository_snapshot, "snapshot-detail");
    assert!(first.area_scores.is_empty());
    assert!(first.project_score.is_none());
    assert!(first.suggestions.is_empty());
    assert!(first.events.is_empty());
    assert_eq!(
        first
            .areas
            .iter()
            .map(|area| area.area.area_key.as_str())
            .collect::<Vec<_>>(),
        vec!["backend", "frontend"]
    );
    let front = first
        .areas
        .iter()
        .find(|area| area.area.id == "detail-front")
        .expect("front");
    assert_eq!(
        front
            .attempts
            .iter()
            .map(|attempt| (attempt.attempt.id.as_str(), attempt.is_current))
            .collect::<Vec<_>>(),
        vec![("old", false), ("current", true)]
    );
    assert_eq!(
        front
            .accepted_findings
            .iter()
            .map(|finding| finding.guardrail_key.as_str())
            .collect::<Vec<_>>(),
        vec!["current"]
    );
    assert_eq!(front.accepted_outputs.len(), 1);
    assert_eq!(
        front.accepted_outputs[0].result["errors"][0]["message"],
        "preserved"
    );
}

#[tokio::test]
async fn run_detail_includes_ordered_terminal_aggregation_suggestions_and_events() {
    let db = Database::ephemeral().await.expect("postgres");
    let project = "readiness-terminal-detail";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let repo = ReadinessRepository::new(db.clone());
    let run = repo
        .create_run(CreateReadinessRun {
            project_id: project.into(),
            idempotency_key: "terminal-detail".into(),
            repository_snapshot: "snapshot-terminal".into(),
            skill_name: "skill-terminal".into(),
            skill_version: "2.0.0".into(),
        })
        .await
        .expect("run");
    for (area_id, area_key, attempt_id) in [
        ("terminal-z", "zeta", "terminal-z-attempt"),
        ("terminal-a", "alpha", "terminal-a-attempt"),
    ] {
        sqlx::query("INSERT INTO readiness_composition_areas (id,run_id,area_key,composition,path_scopes) VALUES ($1,$2,$3,'{}','[]')")
            .bind(area_id).bind(&run.id).bind(area_key).execute(db.pool()).await.expect("area");
        sqlx::query("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,1,$4)")
            .bind(attempt_id).bind(&run.id).bind(area_id).bind(format!("key-{area_id}")).execute(db.pool()).await.expect("attempt");
        sqlx::query("UPDATE readiness_composition_areas SET current_attempt_id=$1 WHERE id=$2")
            .bind(attempt_id)
            .bind(area_id)
            .execute(db.pool())
            .await
            .expect("current attempt");
    }
    for (area_id, score, status) in [
        ("terminal-z", 0.5, "supported"),
        ("terminal-a", 0.0, "unsupported"),
    ] {
        sqlx::query("INSERT INTO readiness_area_scores (run_id,area_id,score,applicable_weight,covered_weight,status) VALUES ($1,$2,$3,2,1,$4)")
            .bind(&run.id).bind(area_id).bind(score).bind(status).execute(db.pool()).await.expect("area score");
    }
    for (area_id, attempt_id) in [
        ("terminal-z", "terminal-z-attempt"),
        ("terminal-a", "terminal-a-attempt"),
    ] {
        sqlx::query("UPDATE readiness_area_attempts SET status='succeeded',terminal_at='2026-01-01T00:00:00.000Z' WHERE id=$1")
            .bind(attempt_id)
            .execute(db.pool())
            .await
            .expect("terminal attempt");
        sqlx::query("UPDATE readiness_composition_areas SET status='succeeded' WHERE id=$1")
            .bind(area_id)
            .execute(db.pool())
            .await
            .expect("terminal area");
    }
    sqlx::query(
        "INSERT INTO readiness_project_scores (run_id,score,band) VALUES ($1,0.5,'emerging')",
    )
    .bind(&run.id)
    .execute(db.pool())
    .await
    .expect("project score");
    for (id, key) in [("suggestion-z", "z-key"), ("suggestion-a", "a-key")] {
        sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ($1,$2,$3,'{}')")
            .bind(id).bind(&run.id).bind(key).execute(db.pool()).await.expect("canonical suggestion");
    }
    for id in ["event-z", "event-a"] {
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload,created_at) VALUES ($1,$2,'lifecycle','{}','2026-01-01T00:00:00.000Z')")
            .bind(id).bind(&run.id).execute(db.pool()).await.expect("event");
    }
    sqlx::query("UPDATE readiness_runs SET expected_area_count=2,status='completed',completed_at='2026-01-01T00:00:01.000Z' WHERE id=$1")
        .bind(&run.id).execute(db.pool()).await.expect("terminal run");

    let first = repo
        .run_detail(&run.id)
        .await
        .expect("detail")
        .expect("run");
    let second = repo
        .run_detail(&run.id)
        .await
        .expect("detail")
        .expect("run");
    assert_eq!(first, second);
    assert_eq!(
        first
            .area_scores
            .iter()
            .map(|score| (score.area_id.as_str(), score.status.as_str()))
            .collect::<Vec<_>>(),
        vec![("terminal-a", "unsupported"), ("terminal-z", "supported")]
    );
    assert_eq!(first.project_score.expect("project score").band, "emerging");
    assert_eq!(
        first
            .suggestions
            .iter()
            .map(|suggestion| suggestion.dedupe_key.as_str())
            .collect::<Vec<_>>(),
        vec!["a-key", "z-key"]
    );
    assert_eq!(
        first
            .events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        vec!["event-a", "event-z"]
    );
}

#[tokio::test]
async fn create_attempt_persists_current_identity_for_run_detail() {
    let db = Database::ephemeral().await.expect("postgres");
    let project = "readiness-detail-create-attempt";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let repo = ReadinessRepository::new(db);
    let run = repo
        .create_run(CreateReadinessRun {
            project_id: project.into(),
            idempotency_key: "detail-create-attempt".into(),
            repository_snapshot: "snapshot-detail".into(),
            skill_name: "skill-detail".into(),
            skill_version: "2.0.0".into(),
        })
        .await
        .expect("run");
    let area = repo
        .create_area(CreateReadinessCompositionArea {
            run_id: run.id.clone(),
            area_key: "backend".into(),
            composition: serde_json::json!({"languages": ["Rust"]}),
            path_scopes: serde_json::json!(["server/**"]),
        })
        .await
        .expect("area");
    let attempt = repo
        .create_attempt(CreateReadinessAreaAttempt {
            run_id: run.id.clone(),
            area_id: area.id.clone(),
            attempt_number: 1,
            correlation_key: "detail-create-attempt-key".into(),
        })
        .await
        .expect("attempt");
    repo.accept_result(
        &attempt,
        &[NewReadinessFinding {
            guardrail_key: "auth".into(),
            status: "covered".into(),
            severity: "high".into(),
            confidence: 0.9,
            evidence: serde_json::json!({"files": ["server/src/auth.rs"]}),
        }],
        &[],
        NewReadinessEvent {
            event_kind: "detail_create_attempt_accepted".into(),
            payload: serde_json::json!({}),
        },
    )
    .await
    .expect("accept result");

    let detail = repo
        .run_detail(&run.id)
        .await
        .expect("detail")
        .expect("run exists");
    assert_eq!(detail.areas.len(), 1);
    assert_eq!(
        detail.areas[0]
            .attempts
            .iter()
            .filter(|attempt| attempt.is_current)
            .count(),
        1
    );
    assert_eq!(detail.areas[0].attempts[0].attempt.id, attempt.id);
    assert!(detail.areas[0].attempts[0].is_current);
    assert_eq!(
        detail.areas[0]
            .accepted_findings
            .iter()
            .map(|finding| finding.guardrail_key.as_str())
            .collect::<Vec<_>>(),
        vec!["auth"]
    );
}

#[derive(sqlx::FromRow)]
struct PersistedAreaTask {
    project_id: String,
    owner: String,
    description: String,
    agent_type: Option<String>,
    execution_context: Option<TaskExecutionContext>,
}

fn identification_output() -> ReadinessIdentificationOutput {
    ReadinessIdentificationOutput {
        areas: vec![
            ReadinessIdentifiedArea {
                area_key: "frontend".into(),
                path_scopes: vec!["apps/web/**".into()],
                languages: vec!["TypeScript".into()],
                roles: vec!["frontend".into()],
                frameworks: vec!["React".into()],
                key_libraries: vec!["vite".into()],
                confidence: 0.9,
                evidence: vec!["apps/web/package.json".into()],
            },
            ReadinessIdentifiedArea {
                area_key: "backend".into(),
                path_scopes: vec!["server/**".into()],
                languages: vec!["Rust".into()],
                roles: vec!["api".into()],
                frameworks: vec!["Axum".into()],
                key_libraries: vec!["sqlx".into()],
                confidence: 0.95,
                evidence: vec!["server/Cargo.toml".into()],
            },
        ],
    }
}

async fn fanout_counts(db: &Database, run_id: &str) -> (i64, i64, i64) {
    let areas =
        sqlx::query_scalar("SELECT COUNT(*) FROM readiness_composition_areas WHERE run_id=$1")
            .bind(run_id)
            .fetch_one(db.pool())
            .await
            .expect("count areas");
    let attempts =
        sqlx::query_scalar("SELECT COUNT(*) FROM readiness_area_attempts WHERE run_id=$1")
            .bind(run_id)
            .fetch_one(db.pool())
            .await
            .expect("count attempts");
    let tasks = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END)->>'kind'='readiness_area_analysis' AND (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END)->>'run_id'=$1").bind(run_id).fetch_one(db.pool()).await.expect("count analysis tasks");
    (areas, attempts, tasks)
}

async fn assert_failed_without_fanout(db: &Database, run_id: &str, reason: &str) {
    assert_eq!(fanout_counts(db, run_id).await, (0, 0, 0));
    let row: (String, serde_json::Value) = sqlx::query_as("SELECT r.status,e.payload FROM readiness_runs r JOIN readiness_run_events e ON e.run_id=r.id WHERE r.id=$1 AND e.event_kind='identification_failed'")
        .bind(run_id).fetch_one(db.pool()).await.expect("failed run event");
    assert_eq!(row.0, "failed");
    assert_eq!(row.1["reason"].as_str(), Some(reason));
}

async fn seed_user(db: &Database, github_id: i64, login: &str) -> String {
    UserRepository::new(db.clone())
        .upsert_from_github(github_id, login, Some(login), None)
        .await
        .expect("seed readiness creator")
        .id
}

fn kickoff_input(
    project_id: &str,
    creator_user_id: &str,
    idempotency_key: &str,
) -> MaterializeReadinessKickoff {
    MaterializeReadinessKickoff {
        project_id: project_id.into(),
        creator_user_id: creator_user_id.into(),
        idempotency_key: idempotency_key.into(),
        repository_snapshot: "sha256:readiness-fixture".into(),
        skill_name: "agent-readiness-guardrails".into(),
        skill_version: "1.0.0".into(),
    }
}

async fn kickoff_counts(db: &Database, project_id: &str) -> (i64, i64) {
    let runs =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM readiness_runs WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(db.pool())
            .await
            .expect("count readiness runs");
    let identification_tasks = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM tasks
         WHERE project_id = $1
           AND (CASE WHEN description LIKE '{%' THEN description::jsonb ELSE '{}'::jsonb END)
               ->> 'kind' = 'readiness_identification'",
    )
    .bind(project_id)
    .fetch_one(db.pool())
    .await
    .expect("count identification tasks");
    (runs, identification_tasks)
}

fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read migrations")
        .filter_map(|entry| {
            let path = entry.expect("migration entry").path();
            let name = path.file_name()?.to_str()?;
            let version = name.split_once('_')?.0.parse().ok()?;
            (path.extension()?.to_str()? == "sql").then_some((version, path))
        })
        .collect();
    entries.sort_by_key(|(version, _)| *version);
    entries
}

async fn apply_prior_migrations(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        // Migration 142 deliberately requires a validated operator GUC even
        // when this fresh fixture has no tasks to backfill.
        if version == 142 {
            sqlx::query(
                "INSERT INTO users (id,github_id,github_login,is_member_of_org) \
                 VALUES ($1,155,'readiness-migration-operator',true)",
            )
            .bind(DESIGNATED_OPERATOR_ID)
            .execute(&mut *conn)
            .await
            .expect("seed migration 142 designated operator");
            sqlx::query(
                "SELECT set_config('djinn.migration_designated_operator_user_id',$1,false)",
            )
            .bind(DESIGNATED_OPERATOR_ID)
            .execute(&mut *conn)
            .await
            .expect("set migration 142 designated operator");
        }
        conn.execute(
            std::fs::read_to_string(&path)
                .expect("read migration")
                .as_str(),
        )
        .await
        .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    }
}

async fn apply_readiness_migration(conn: &mut PgConnection) {
    for migration in [
        MIGRATION_FILE,
        FINDING_CONFIDENCE_MIGRATION_FILE,
        RESULT_OUTPUTS_MIGRATION_FILE,
        CURRENT_ATTEMPT_MIGRATION_FILE,
    ] {
        let path = migrations_dir().join(migration);
        conn.execute(
            std::fs::read_to_string(&path)
                .expect("read readiness migration")
                .as_str(),
        )
        .await
        .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    }
}

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = djinn_db::test_database_base_url();
    let prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base);
    let name = format!("djinn_readiness_{suffix}_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin connect");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create test database");
    drop(admin);

    let database_url = format!("{prefix}/{name}");
    let result = f(database_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("admin reconnect");
    let _ = admin.execute(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}' AND pid <> pg_backend_pid()").as_str()).await;
    let _ = admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{name}""#).as_str())
        .await;
    result
}

async fn migrated_connection(url: &str) -> PgConnection {
    let mut conn = PgConnection::connect(url)
        .await
        .expect("connect fresh database");
    apply_prior_migrations(&mut conn).await;
    apply_readiness_migration(&mut conn).await;
    conn
}

async fn seed_project(conn: &mut PgConnection, id: &str) {
    sqlx::query(
        "INSERT INTO projects (id,name,github_owner,github_repo) VALUES ($1,$2,'djinnos',$3)",
    )
    .bind(id)
    .bind(format!("project-{id}"))
    .bind(format!("repo-{id}"))
    .execute(&mut *conn)
    .await
    .expect("seed project");
}

async fn run(conn: &mut PgConnection, id: &str, project: &str, key: &str) {
    sqlx::query("INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ($1,$2,$3,'snapshot','skill','1.0.0')")
        .bind(id).bind(project).bind(key).execute(&mut *conn).await.expect("insert run");
}

async fn area(conn: &mut PgConnection, id: &str, run_id: &str, key: &str) {
    sqlx::query("INSERT INTO readiness_composition_areas (id,run_id,area_key,composition,path_scopes) VALUES ($1,$2,$3,'{}','[]')")
        .bind(id).bind(run_id).bind(key).execute(&mut *conn).await.expect("insert area");
}

async fn attempt(
    conn: &mut PgConnection,
    id: &str,
    run_id: &str,
    area_id: &str,
    number: i32,
    correlation: &str,
) {
    sqlx::query("INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ($1,$2,$3,$4,$5)")
        .bind(id).bind(run_id).bind(area_id).bind(number).bind(correlation)
        .execute(&mut *conn).await.expect("insert attempt");
}

async fn assert_rejected(conn: &mut PgConnection, sql: &str, marker: &str) {
    let error = conn.execute(sql).await.expect_err("write must be rejected");
    assert!(
        error.to_string().contains(marker),
        "expected {marker}, got {error}"
    );
}

#[tokio::test]
async fn migration_constraints_reject_identity_correlation_and_lifecycle_violations() {
    with_temp_database("constraints", |url| async move {
        let mut conn = migrated_connection(&url).await;
        seed_project(&mut conn, "project-a").await;
        seed_project(&mut conn, "project-b").await;
        run(&mut conn, "run-a", "project-a", "key-a").await;

        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ('run-duplicate','project-a','key-a','s','s','1')", "readiness_runs_project_idempotency_key").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,repository_snapshot,skill_name,skill_version) VALUES ('run-active','project-a','key-b','s','s','1')", "readiness_runs_one_active_project_idx").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version) VALUES ('bad-status','project-b','bad','unknown','s','s','1')", "readiness_runs_status_check").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_runs (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version) VALUES ('bad-terminal','project-b','terminal','completed','s','s','1')", "readiness_runs_terminal_check").await;

        area(&mut conn, "area-a", "run-a", "frontend").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_composition_areas (id,run_id,area_key) VALUES ('area-duplicate','run-a','frontend')", "readiness_areas_run_key").await;
        attempt(&mut conn, "attempt-a", "run-a", "area-a", 1, "correlation-a").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('attempt-number','run-a','area-a',1,'correlation-b')", "readiness_attempts_area_number").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('attempt-correlation','run-a','area-a',2,'correlation-a')", "readiness_attempts_correlation_key").await;
        assert_rejected(&mut conn, "UPDATE readiness_area_attempts SET status='succeeded' WHERE id='attempt-a'", "readiness_attempts_terminal_check").await;

        run(&mut conn, "run-b", "project-b", "key-b").await;
        area(&mut conn, "area-b", "run-b", "backend").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('cross-attempt','run-b','area-a',3,'cross-run-correlation')", "readiness_attempts_area_run_fk").await;
        sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity,accepted) VALUES ('finding-a','run-a','area-a','attempt-a','guardrail','high',true)").execute(&mut conn).await.expect("insert finding");
        assert_rejected(&mut conn, "INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('finding-duplicate','run-a','area-a','attempt-a','guardrail','high')", "readiness_findings_attempt_guardrail").await;
        assert_rejected(&mut conn, "INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('finding-cross','run-b','area-b','attempt-a','other','high')", "readiness_findings_attempt_correlation_fk").await;
        sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('suggestion-a','run-a','dedupe','{}')").execute(&mut conn).await.expect("insert suggestion");
        assert_rejected(&mut conn, "INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('suggestion-duplicate','run-a','dedupe','{}')", "readiness_suggestions_run_dedupe").await;
    }).await;
}

#[tokio::test]
async fn result_output_constraints_enforce_identity_and_completed_run_immutability() {
    with_temp_database("result-output-constraints", |url| async move {
        let mut conn = migrated_connection(&url).await;
        seed_project(&mut conn, "output-project-a").await;
        seed_project(&mut conn, "output-project-b").await;
        seed_project(&mut conn, "output-terminal-project").await;

        run(&mut conn, "output-run-a", "output-project-a", "output-key-a").await;
        area(&mut conn, "output-area-a", "output-run-a", "output-area-key-a").await;
        attempt(&mut conn, "output-attempt-a", "output-run-a", "output-area-a", 1, "output-correlation-a").await;
        run(&mut conn, "output-run-b", "output-project-b", "output-key-b").await;
        area(&mut conn, "output-area-b", "output-run-b", "output-area-key-b").await;
        attempt(&mut conn, "output-attempt-b", "output-run-b", "output-area-b", 1, "output-correlation-b").await;

        // This attempt has no output row yet, so the composite FK—not the
        // output primary key—must reject its mismatched run and area values.
        assert_rejected(&mut conn, "INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ('output-run-b','output-area-b','output-attempt-a','{}')", "readiness_area_result_outputs_attempt_id_run_id_area_id_fkey").await;
        sqlx::query("INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ('output-run-a','output-area-a','output-attempt-a','{}')").execute(&mut conn).await.expect("insert correlated output");
        assert_rejected(&mut conn, "INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ('output-run-a','output-area-a','output-attempt-a','{\"duplicate\":true}')", "readiness_area_result_outputs_pkey").await;

        run(&mut conn, "output-terminal-run", "output-terminal-project", "output-terminal-key").await;
        area(&mut conn, "output-terminal-area", "output-terminal-run", "output-terminal-area-key").await;
        attempt(&mut conn, "output-terminal-attempt", "output-terminal-run", "output-terminal-area", 1, "output-terminal-correlation").await;
        attempt(&mut conn, "output-terminal-insert-attempt", "output-terminal-run", "output-terminal-area", 2, "output-terminal-insert-correlation").await;
        sqlx::query("INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ('output-terminal-run','output-terminal-area','output-terminal-attempt','{}')").execute(&mut conn).await.expect("seed output before completion");
        sqlx::query("UPDATE readiness_composition_areas SET status='succeeded' WHERE id='output-terminal-area'").execute(&mut conn).await.expect("terminalize output area");
        sqlx::query("UPDATE readiness_runs SET expected_area_count=1,status='completed',completed_at='2025-01-01T00:00:00.000Z' WHERE id='output-terminal-run'").execute(&mut conn).await.expect("complete output run");

        assert_rejected(&mut conn, "INSERT INTO readiness_area_result_outputs (run_id,area_id,attempt_id,result) VALUES ('output-terminal-run','output-terminal-area','output-terminal-insert-attempt','{}')", "readiness run is terminal").await;
        assert_rejected(&mut conn, "UPDATE readiness_area_result_outputs SET result='{\"changed\":true}' WHERE attempt_id='output-terminal-attempt'", "readiness run is terminal").await;
        assert_rejected(&mut conn, "DELETE FROM readiness_area_result_outputs WHERE attempt_id='output-terminal-attempt'", "readiness run is terminal").await;
    }).await;
}

#[tokio::test]
async fn repository_concurrent_start_has_one_active_run_and_same_key_resolves() {
    // Database::ephemeral is a template-cloned, real Postgres database.
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    djinn_db::test_support::seed_project(&db, "readiness-concurrency", "readiness").await;
    let repo = ReadinessRepository::new(db.clone());
    let input = |key: &str| CreateReadinessRun {
        project_id: "readiness-concurrency".into(),
        idempotency_key: key.into(),
        repository_snapshot: "snapshot".into(),
        skill_name: "skill".into(),
        skill_version: "1.0.0".into(),
    };
    let (left, right) = tokio::join!(
        repo.create_run(input("left")),
        repo.create_run(input("right"))
    );
    assert_eq!(
        usize::from(left.is_ok()) + usize::from(right.is_ok()),
        1,
        "only one active run may be created"
    );
    let winner = left.as_ref().ok().or(right.as_ref().ok()).expect("winner");
    let duplicate = repo
        .create_run(input(&winner.idempotency_key))
        .await
        .expect("same idempotency key resolves");
    assert_eq!(duplicate.id, winner.id);
    let active = repo
        .active_or_latest_for_project("readiness-concurrency")
        .await
        .expect("load active run")
        .expect("active run exists");
    assert_eq!(
        active.id, winner.id,
        "active run is preferred by repository query"
    );

    // Once the sole active row becomes terminal, the same repository method
    // must fall back to the most recently-created terminal run rather than an
    // arbitrary (or oldest) project run.
    sqlx::query(
        "UPDATE readiness_runs SET status='failed', completed_at='2026-01-01T00:00:00.000Z', \
         created_at='2026-01-01T00:00:00.000Z' WHERE id=$1",
    )
    .bind(&winner.id)
    .execute(db.pool())
    .await
    .expect("terminalize active run");
    sqlx::query(
        "INSERT INTO readiness_runs \
         (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,created_at,completed_at) \
         VALUES ('older-terminal','readiness-concurrency','older','failed','snapshot','skill','1.0.0', \
                 '2025-01-01T00:00:00.000Z','2025-01-01T00:00:00.000Z')",
    )
    .execute(db.pool())
    .await
    .expect("create older terminal run");
    let latest_terminal = repo
        .active_or_latest_for_project("readiness-concurrency")
        .await
        .expect("load latest terminal run")
        .expect("terminal run exists");
    assert_eq!(
        latest_terminal.id, winner.id,
        "repository falls back to the newest terminal run when no run is active"
    );
}

#[tokio::test]
async fn repository_active_or_latest_selection_is_deterministic() {
    // Database::ephemeral is a template-cloned, real Postgres database.
    let db = Database::ephemeral()
        .await
        .expect("open postgres test database");
    let project = "readiness-selector";
    djinn_db::test_support::seed_project(&db, project, project).await;
    let repo = ReadinessRepository::new(db.clone());

    for (id, key, status, created_at) in [
        (
            "selector-terminal-newer",
            "terminal-newer",
            "failed",
            "2026-01-03T00:00:00.000Z",
        ),
        (
            "selector-active",
            "active",
            "identifying",
            "2026-01-01T00:00:00.000Z",
        ),
    ] {
        sqlx::query(
            "INSERT INTO readiness_runs \
             (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,created_at,completed_at) \
             VALUES ($1,$2,$3,$4,'snapshot','skill','1.0.0',$5, \
                     CASE WHEN $4 = 'identifying' THEN NULL ELSE $5 END)",
        )
        .bind(id)
        .bind(project)
        .bind(key)
        .bind(status)
        .bind(created_at)
        .execute(db.pool())
        .await
        .expect("seed selector candidate");
    }

    for status in ["identifying", "analyzing", "aggregating"] {
        sqlx::query("UPDATE readiness_runs SET status=$1 WHERE id='selector-active'")
            .bind(status)
            .execute(db.pool())
            .await
            .expect("set active selector status");
        let active = repo
            .active_or_latest_for_project(project)
            .await
            .expect("select active run")
            .expect("active run exists");
        assert_eq!(
            active.id, "selector-active",
            "the {status} active run wins even when a terminal run is newer"
        );
    }

    sqlx::query(
        "UPDATE readiness_runs \
         SET status='failed', completed_at='2026-01-04T00:00:00.000Z' WHERE id='selector-active'",
    )
    .execute(db.pool())
    .await
    .expect("terminalize active selector candidate");
    let latest_terminal = repo
        .active_or_latest_for_project(project)
        .await
        .expect("select latest terminal run")
        .expect("terminal run exists");
    assert_eq!(
        latest_terminal.id, "selector-terminal-newer",
        "without an active run, the newest terminal run wins"
    );

    for (id, key) in [("selector-tie-a", "tie-a"), ("selector-tie-z", "tie-z")] {
        sqlx::query(
            "INSERT INTO readiness_runs \
             (id,project_id,idempotency_key,status,repository_snapshot,skill_name,skill_version,created_at,completed_at) \
             VALUES ($1,$2,$3,'failed','snapshot','skill','1.0.0', \
                     '2026-01-05T00:00:00.000Z','2026-01-05T00:00:00.000Z')",
        )
        .bind(id)
        .bind(project)
        .bind(key)
        .execute(db.pool())
        .await
        .expect("seed equal-timestamp selector candidate");
    }
    for _ in 0..5 {
        let selected = repo
            .active_or_latest_for_project(project)
            .await
            .expect("select tied terminal run")
            .expect("tied terminal runs exist");
        assert_eq!(
            selected.id, "selector-tie-z",
            "equal timestamps resolve by descending stable run id"
        );
    }
}

#[tokio::test]
async fn active_latest_and_detail_indexes_have_expected_query_paths() {
    with_temp_database("indexes", |url| async move {
        let mut conn = migrated_connection(&url).await;
        seed_project(&mut conn, "project-index").await;
        run(&mut conn, "run-old", "project-index", "old").await;
        sqlx::query("UPDATE readiness_runs SET status='failed', completed_at='2025-01-01T00:00:00.000Z', created_at='2025-01-01T00:00:00.000Z' WHERE id='run-old'").execute(&mut conn).await.expect("terminalize old");
        run(&mut conn, "run-active", "project-index", "active").await;
        area(&mut conn, "area-index", "run-active", "ui").await;
        attempt(&mut conn, "attempt-index", "run-active", "area-index", 1, "index-correlation").await;
        let selected: String = sqlx::query_scalar("SELECT id FROM readiness_runs WHERE project_id=$1 ORDER BY (status IN ('identifying','analyzing','aggregating')) DESC,created_at DESC LIMIT 1").bind("project-index").fetch_one(&mut conn).await.expect("active/latest query");
        assert_eq!(selected, "run-active", "active run wins over a terminal latest candidate");
        conn.execute("SET enable_seqscan = off").await.expect("force index plans");
        for (sql, indexes) in [
            ("EXPLAIN SELECT * FROM readiness_runs WHERE project_id='project-index' AND status IN ('identifying','analyzing','aggregating')", &["readiness_runs_one_active_project_idx"][..]),
            ("EXPLAIN SELECT * FROM readiness_runs WHERE project_id='project-index' ORDER BY created_at DESC", &["readiness_runs_project_latest_idx"][..]),
            ("EXPLAIN SELECT * FROM readiness_composition_areas WHERE run_id='run-active' ORDER BY area_key", &["readiness_areas_run_detail_idx", "readiness_areas_current_attempt_idx"][..]),
            ("EXPLAIN SELECT * FROM readiness_area_attempts WHERE area_id='area-index' ORDER BY attempt_number DESC", &["readiness_attempts_area_idx"][..]),
            ("EXPLAIN SELECT * FROM readiness_run_events WHERE run_id='run-active' ORDER BY created_at,id", &["readiness_events_run_detail_idx"][..]),
        ] {
            let plan: Vec<String> = sqlx::query_scalar(sql).fetch_all(&mut conn).await.expect("explain query path");
            let plan_text = plan.join("\n");
            assert!(
                indexes.iter().any(|index| plan_text.contains(index)),
                "expected one of {indexes:?} in plan: {plan:?}"
            );
        }
    }).await;
}

#[tokio::test]
async fn frozen_and_completed_readiness_data_is_immutable() {
    with_temp_database("immutable", |url| async move {
        let mut conn = migrated_connection(&url).await;
        seed_project(&mut conn, "project-immutable").await;
        run(&mut conn, "run-immutable", "project-immutable", "immutable").await;
        area(&mut conn, "area-immutable", "run-immutable", "frozen").await;
        sqlx::query("UPDATE readiness_composition_areas SET status='running' WHERE id='area-immutable'").execute(&mut conn).await.expect("status transition allowed");
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET composition='{\"changed\":true}' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET path_scopes='[\"changed\"]' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET area_key='changed' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        assert_rejected(&mut conn, "UPDATE readiness_composition_areas SET run_id='other-run' WHERE id='area-immutable'", "readiness composition area is frozen").await;
        attempt(&mut conn, "attempt-immutable", "run-immutable", "area-immutable", 1, "immutable-correlation").await;
        sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity,accepted) VALUES ('finding-immutable','run-immutable','area-immutable','attempt-immutable','guardrail','high',true)").execute(&mut conn).await.expect("accepted finding");
        assert_rejected(&mut conn, "UPDATE readiness_guardrail_findings SET severity='low' WHERE id='finding-immutable'", "accepted readiness finding is immutable").await;
        assert_rejected(&mut conn, "DELETE FROM readiness_guardrail_findings WHERE id='finding-immutable'", "accepted readiness finding is immutable").await;
        sqlx::query("INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('finding-unaccepted','run-immutable','area-immutable','attempt-immutable','unaccepted','low')").execute(&mut conn).await.expect("unaccepted finding");
        sqlx::query("INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('suggestion-immutable','run-immutable','dedupe','{}')").execute(&mut conn).await.expect("suggestion");
        sqlx::query("INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ('event-immutable','run-immutable','created','{}')").execute(&mut conn).await.expect("event");
        assert_rejected(&mut conn, "UPDATE readiness_run_events SET event_kind='changed' WHERE id='event-immutable'", "readiness run events are append-only").await;
        sqlx::query("UPDATE readiness_area_attempts SET status='succeeded', terminal_at='2025-01-01T00:00:00.000Z' WHERE id='attempt-immutable'").execute(&mut conn).await.expect("terminal attempt");
        sqlx::query("UPDATE readiness_composition_areas SET status='succeeded' WHERE id='area-immutable'").execute(&mut conn).await.expect("terminal area");
        sqlx::query("UPDATE readiness_runs SET expected_area_count=1,status='completed',completed_at='2025-01-01T00:00:00.000Z' WHERE id='run-immutable'").execute(&mut conn).await.expect("complete run");
        for sql in [
            "INSERT INTO readiness_composition_areas (id,run_id,area_key) VALUES ('after-area','run-immutable','after')",
            "UPDATE readiness_composition_areas SET status='failed' WHERE id='area-immutable'",
            "DELETE FROM readiness_composition_areas WHERE id='area-immutable'",
            "INSERT INTO readiness_area_attempts (id,run_id,area_id,attempt_number,correlation_key) VALUES ('after-attempt','run-immutable','area-immutable',2,'after')",
            "UPDATE readiness_area_attempts SET payload_digest='changed' WHERE id='attempt-immutable'",
            "DELETE FROM readiness_area_attempts WHERE id='attempt-immutable'",
            "INSERT INTO readiness_guardrail_findings (id,run_id,area_id,attempt_id,guardrail_key,severity) VALUES ('after-finding','run-immutable','area-immutable','attempt-immutable','after','low')",
            "UPDATE readiness_guardrail_findings SET severity='high' WHERE id='finding-unaccepted'",
            "INSERT INTO readiness_remediation_suggestions (id,run_id,dedupe_key,suggestion) VALUES ('after-suggestion','run-immutable','after','{}')",
            "DELETE FROM readiness_remediation_suggestions WHERE id='suggestion-immutable'",
            "INSERT INTO readiness_run_events (id,run_id,event_kind,payload) VALUES ('after-event','run-immutable','after','{}')",
        ] { assert_rejected(&mut conn, sql, "readiness run is terminal").await; }

        // The append-only trigger is deliberately stronger than the terminal
        // child-write trigger for deletes, so assert its stable contract here.
        assert_rejected(&mut conn, "DELETE FROM readiness_run_events WHERE id='event-immutable'", "readiness run events are append-only").await;

        // Isolate the completed-run finding delete guard from the independent
        // guard which rejects every finding delete (accepted or otherwise).
        // The unaccepted update above exercises the normal trigger stack.
        conn.execute("ALTER TABLE readiness_guardrail_findings DISABLE TRIGGER readiness_findings_immutable_delete").await.expect("disable independent finding delete guard");
        assert_rejected(&mut conn, "DELETE FROM readiness_guardrail_findings WHERE id='finding-unaccepted'", "readiness run is terminal").await;
        conn.execute("ALTER TABLE readiness_guardrail_findings ENABLE TRIGGER readiness_findings_immutable_delete").await.expect("restore independent finding delete guard");
        assert_rejected(&mut conn, "UPDATE readiness_runs SET status='failed' WHERE id='run-immutable'", "completed readiness run is immutable").await;
    }).await;
}
