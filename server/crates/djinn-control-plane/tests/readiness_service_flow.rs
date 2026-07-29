//! Real-Postgres consumer service flows for persisted readiness analysis.
//!
//! Consumer setup and reads deliberately cross only the kickoff/query service
//! boundary. Architect callback simulation uses the repository API so this test
//! never reaches into readiness tables directly.

use async_trait::async_trait;
use djinn_control_plane::{
    readiness_kickoff::{
        READINESS_SKILL_NAME, READINESS_SKILL_VERSION, ReadinessKickoffRequest,
        ReadinessKickoffService, ReadinessSkillPinError, ReadinessSkillPinResolver,
    },
    readiness_query::{ReadinessQueryService, ReadinessRunQuery},
};
use djinn_core::events::EventBus;
use djinn_db::{
    Database, ProjectRepository, ReadinessAreaResultCallback, ReadinessCallbackOutcome,
    ReadinessRepository, RepoGraphCacheInsert, RepoGraphCacheRepository, UserRepository,
    repositories::readiness::{
        ReadinessAreaFanout, ReadinessIdentificationOutput, ReadinessIdentifiedArea,
    },
};

#[derive(Clone)]
struct FixturePinResolver;

#[async_trait]
impl ReadinessSkillPinResolver for FixturePinResolver {
    async fn resolve_exact(
        &self,
        name: &'static str,
        version: &'static str,
    ) -> Result<(), ReadinessSkillPinError> {
        assert_eq!(name, READINESS_SKILL_NAME);
        assert_eq!(version, READINESS_SKILL_VERSION);
        Ok(())
    }
}

#[tokio::test]
async fn typescript_and_rust_service_flow_projects_terminal_detail_and_scores() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id) = seed_project_and_owner(&db, "readiness-service-flow").await;
    seed_snapshot(&db, &project_id, "1111111111111111111111111111111111111111").await;

    let kickoff = kickoff(&db, &project_id, &owner_id, "first-flow").await;
    let repository = ReadinessRepository::new(db.clone());
    let fanout = repository
        .complete_identification(&kickoff.run.id, &owner_id, two_area_composition())
        .await
        .expect("freeze TypeScript and Rust areas");
    assert_eq!(fanout.len(), 2);

    let frontend = fanout
        .iter()
        .find(|item| item.area.area_key == "frontend")
        .expect("TypeScript frontend fanout");
    let backend = fanout
        .iter()
        .find(|item| item.area.area_key == "backend")
        .expect("Rust backend fanout");
    assert_eq!(
        repository
            .ingest_area_result(callback(&kickoff.run.id, frontend, frontend_result()))
            .await
            .expect("accept frontend correlation"),
        ReadinessCallbackOutcome::Accepted
    );
    assert_eq!(
        repository
            .ingest_area_result(callback(&kickoff.run.id, backend, backend_result()))
            .await
            .expect("accept backend correlation"),
        ReadinessCallbackOutcome::Accepted
    );
    let aggregation = repository
        .aggregate_run(&kickoff.run.id, "fixture-aggregator")
        .await
        .expect("terminal aggregation");
    assert_eq!(aggregation.status, "completed");

    let query = ReadinessQueryService::new(db.clone());
    let detail = query_detail(&query, &project_id, &kickoff.run.id, &owner_id).await;
    assert_eq!(detail.run.status, "completed");
    assert_eq!(detail.run.skill_name, READINESS_SKILL_NAME);
    assert_eq!(detail.run.skill_version, READINESS_SKILL_VERSION);
    assert_eq!(detail.run.expected_area_count, Some(2));
    assert_eq!(detail.areas.len(), 2);
    assert_eq!(
        detail
            .areas
            .iter()
            .map(|area| area.area_key.as_str())
            .collect::<Vec<_>>(),
        vec!["backend", "frontend"]
    );

    let frontend_detail = detail
        .areas
        .iter()
        .find(|area| area.area_key == "frontend")
        .expect("frontend detail");
    assert_eq!(
        frontend_detail.composition["languages"],
        serde_json::json!(["TypeScript"])
    );
    assert_eq!(frontend_detail.attempts.len(), 1);
    assert!(frontend_detail.attempts[0].is_current);
    assert_eq!(frontend_detail.attempts[0].status, "succeeded");
    assert_eq!(frontend_detail.accepted_findings.len(), 4);
    let auth_finding = frontend_detail
        .accepted_findings
        .iter()
        .find(|finding| finding.guardrail_key == "frontend-auth")
        .expect("accepted auth evidence");
    assert_eq!(
        auth_finding.evidence,
        serde_json::json!({"path":"apps/web/auth.ts","line":42})
    );
    assert_eq!(auth_finding.confidence, 0.91);
    assert_eq!(
        frontend_detail.accepted_outputs[0].result["findings"][3]["gap_reason"],
        "session expiry is not enforced"
    );
    assert_eq!(
        frontend_detail.accepted_outputs[0].result["warnings"][0]["warning"],
        "legacy cookie migration remains"
    );
    assert_eq!(
        frontend_detail.accepted_outputs[0].result["errors"][0]["message"],
        "non-fatal source-map lookup failed"
    );

    // frontend: (critical 5 * 1) + (high 3 * .5) + (medium 2 * .5) +
    // (low 1 * 0) = 7.5 / 11. backend: (high 3 + medium 2) = 5 / 5.
    let frontend_score = detail
        .area_scores
        .iter()
        .find(|score| score.area_id == frontend_detail.id)
        .expect("frontend score");
    assert_eq!(
        (
            frontend_score.applicable_weight,
            frontend_score.covered_weight,
            frontend_score.status.as_str()
        ),
        (11, 7.5, "supported")
    );
    assert_float_eq(frontend_score.score, 7.5 / 11.0);
    let backend_score = detail
        .area_scores
        .iter()
        .find(|score| score.area_id == backend.area.id)
        .expect("backend score");
    assert_eq!(
        (
            backend_score.applicable_weight,
            backend_score.covered_weight,
            backend_score.score
        ),
        (5, 5.0, 1.0)
    );
    // Project arithmetic is applicable-weighted: (7.5 + 5) / (11 + 5) = .78125.
    let project_score = detail.project_score.expect("terminal project score");
    assert_float_eq(project_score.score, 12.5 / 16.0);
    assert_eq!(project_score.band, "ready");

    assert_eq!(
        detail.suggestions.len(),
        1,
        "dedupe key has one canonical suggestion"
    );
    let suggestion = &detail.suggestions[0];
    assert_eq!(suggestion.dedupe_key, "rotate-auth-secret");
    assert_eq!(suggestion.suggestion["action"], "rotate shared auth secret");
    let mut expected_area_ids = vec![backend.area.id.clone(), frontend.area.id.clone()];
    expected_area_ids.sort();
    assert_eq!(
        suggestion.suggestion["area_ids"],
        serde_json::json!(expected_area_ids)
    );
    assert_eq!(
        suggestion.suggestion["guardrail_ids"],
        serde_json::json!(["backend-auth", "frontend-auth"])
    );
    assert!(
        detail
            .events
            .iter()
            .any(|event| event.event_kind == "readiness_aggregated")
    );
}

#[tokio::test]
async fn library_only_service_flow_marks_application_guardrails_unsupported() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id) = seed_project_and_owner(&db, "readiness-library-only").await;
    seed_snapshot(&db, &project_id, "2222222222222222222222222222222222222222").await;
    let kickoff = kickoff(&db, &project_id, &owner_id, "library-only").await;
    let repository = ReadinessRepository::new(db.clone());
    let fanout = repository
        .complete_identification(&kickoff.run.id, &owner_id, library_composition())
        .await
        .expect("freeze library composition");
    let library = fanout.first().expect("library fanout");
    assert_eq!(
        repository
            .ingest_area_result(callback(&kickoff.run.id, library, library_result()))
            .await
            .expect("accept library result"),
        ReadinessCallbackOutcome::Accepted
    );
    assert_eq!(
        repository
            .aggregate_run(&kickoff.run.id, "fixture-aggregator")
            .await
            .expect("aggregate unsupported guardrail")
            .status,
        "completed"
    );

    let detail = query_detail(
        &ReadinessQueryService::new(db),
        &project_id,
        &kickoff.run.id,
        &owner_id,
    )
    .await;
    assert_eq!(detail.areas[0].accepted_findings[0].status, "unsupported");
    assert_eq!(
        detail.areas[0].accepted_outputs[0].result["unsupported"][0]["reason"],
        "application authentication is not applicable to this library"
    );
    let area_score = &detail.area_scores[0];
    assert_eq!(
        (
            area_score.applicable_weight,
            area_score.covered_weight,
            area_score.score,
            area_score.status.as_str()
        ),
        (0, 0.0, 0.0, "unsupported")
    );
    let project_score = detail.project_score.expect("project score");
    assert_eq!(
        (project_score.score, project_score.band.as_str()),
        (0.0, "blocked")
    );
}

#[tokio::test]
async fn post_terminal_kickoff_uses_new_snapshot_and_preserves_first_detail() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id) = seed_project_and_owner(&db, "readiness-rerun").await;
    seed_snapshot(&db, &project_id, "3333333333333333333333333333333333333333").await;
    let first = kickoff(&db, &project_id, &owner_id, "first-terminal").await;
    complete_library_run(&db, &first.run.id, &owner_id).await;
    let query = ReadinessQueryService::new(db.clone());
    let before = query_detail(&query, &project_id, &first.run.id, &owner_id).await;
    let before_bytes = serde_json::to_vec(&before).expect("serialize immutable first detail");

    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    seed_snapshot(&db, &project_id, "4444444444444444444444444444444444444444").await;
    let second = kickoff(&db, &project_id, &owner_id, "second-terminal").await;
    assert_ne!(
        first.run.id, second.run.id,
        "new key after terminal appends a run"
    );
    assert_eq!(
        second.run.repository_snapshot,
        "4444444444444444444444444444444444444444"
    );
    assert_eq!(second.run.skill_name, READINESS_SKILL_NAME);
    assert_eq!(second.run.skill_version, READINESS_SKILL_VERSION);
    assert_eq!(
        query_detail(&query, &project_id, &first.run.id, &owner_id).await,
        before,
        "prior completed projection remains value-equal"
    );
    assert_eq!(
        serde_json::to_vec(&query_detail(&query, &project_id, &first.run.id, &owner_id).await)
            .expect("serialize repeated detail"),
        before_bytes,
        "prior completed projection remains byte-for-byte equal"
    );
}

async fn complete_library_run(db: &Database, run_id: &str, owner_id: &str) {
    let repository = ReadinessRepository::new(db.clone());
    let fanout = repository
        .complete_identification(run_id, owner_id, library_composition())
        .await
        .expect("freeze library area");
    let library = fanout.first().expect("library area");
    repository
        .ingest_area_result(callback(run_id, library, library_result()))
        .await
        .expect("accept library callback");
    repository
        .aggregate_run(run_id, "fixture-aggregator")
        .await
        .expect("terminalize library run");
}

async fn kickoff(
    db: &Database,
    project_id: &str,
    owner_id: &str,
    idempotency_key: &str,
) -> djinn_db::ReadinessKickoffMaterialization {
    ReadinessKickoffService::new(db.clone(), FixturePinResolver)
        .kickoff(ReadinessKickoffRequest {
            project_id: project_id.into(),
            authenticated_owner_id: owner_id.into(),
            idempotency_key: idempotency_key.into(),
        })
        .await
        .expect("owner kickoff with persisted context")
}

async fn query_detail(
    query: &ReadinessQueryService,
    project_id: &str,
    run_id: &str,
    owner_id: &str,
) -> djinn_control_plane::readiness_query::ReadinessRunDetailDto {
    query
        .run_detail(ReadinessRunQuery {
            project_id: project_id.into(),
            run_id: run_id.into(),
            authenticated_owner_id: owner_id.into(),
        })
        .await
        .expect("owner queries terminal detail")
}

fn callback(
    run_id: &str,
    fanout: &ReadinessAreaFanout,
    result: serde_json::Value,
) -> ReadinessAreaResultCallback {
    ReadinessAreaResultCallback {
        run_id: run_id.into(),
        area_id: fanout.area.id.clone(),
        attempt_id: fanout.attempt.id.clone(),
        correlation_key: fanout.attempt.correlation_key.clone(),
        task_id: fanout.task.id.clone(),
        status: "succeeded".into(),
        result,
    }
}

fn two_area_composition() -> ReadinessIdentificationOutput {
    ReadinessIdentificationOutput {
        areas: vec![
            identified_area("frontend", "apps/web/**", "TypeScript", "frontend"),
            identified_area("backend", "crates/api/**", "Rust", "backend"),
        ],
    }
}

fn library_composition() -> ReadinessIdentificationOutput {
    ReadinessIdentificationOutput {
        areas: vec![identified_area(
            "library",
            "crates/library/**",
            "Rust",
            "library",
        )],
    }
}

fn identified_area(
    area_key: &str,
    path_scope: &str,
    language: &str,
    role: &str,
) -> ReadinessIdentifiedArea {
    ReadinessIdentifiedArea {
        area_key: area_key.into(),
        path_scopes: vec![path_scope.into()],
        languages: vec![language.into()],
        roles: vec![role.into()],
        frameworks: Vec::new(),
        key_libraries: Vec::new(),
        confidence: 0.95,
        evidence: vec![format!("{path_scope} composition manifest")],
    }
}

fn frontend_result() -> serde_json::Value {
    serde_json::json!({
        "findings": [
            {"guardrail_key":"frontend-auth","status":"covered","severity":"critical","confidence":0.91,"evidence":{"path":"apps/web/auth.ts","line":42}},
            {"guardrail_key":"frontend-csrf","status":"partial","severity":"high","confidence":0.88,"evidence":{"path":"apps/web/csrf.ts"}},
            {"guardrail_key":"frontend-audit","status":"covered","severity":"medium","confidence":0.69,"evidence":{"path":"apps/web/audit.ts"}},
            {"guardrail_key":"frontend-session-expiry","status":"missing","severity":"low","confidence":0.96,"gap_reason":"session expiry is not enforced","evidence":{"path":"apps/web/session.ts"}}
        ],
        "unsupported": [],
        "warnings": [{"warning":"legacy cookie migration remains"}],
        "errors": [{"message":"non-fatal source-map lookup failed"}],
        "remediation_suggestions": [{"dedupe_key":"rotate-auth-secret","action":"rotate shared auth secret","guardrail_id":"frontend-auth"}]
    })
}

fn backend_result() -> serde_json::Value {
    serde_json::json!({
        "findings": [
            {"guardrail_key":"backend-auth","status":"covered","severity":"high","confidence":0.95,"evidence":{"path":"crates/api/src/auth.rs"}},
            {"guardrail_key":"backend-audit","status":"covered","severity":"medium","confidence":0.95,"evidence":{"path":"crates/api/src/audit.rs"}}
        ],
        "unsupported": [],
        "warnings": [{"warning":"rotation job ownership is shared"}],
        "remediation_suggestions": [{"dedupe_key":"rotate-auth-secret","action":"rotate shared auth secret","guardrail_id":"backend-auth"}]
    })
}

fn library_result() -> serde_json::Value {
    serde_json::json!({
        "findings": [{"guardrail_key":"application-auth","status":"unsupported","severity":"critical","confidence":1.0,"evidence":{"path":"crates/library/src/lib.rs"}}],
        "unsupported": [{"guardrail_key":"application-auth","reason":"application authentication is not applicable to this library"}],
        "warnings": [{"warning":"library-only composition"}],
        "remediation_suggestions": []
    })
}

async fn seed_project_and_owner(db: &Database, name: &str) -> (String, String) {
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create(name, "readiness-flow-owner", name)
        .await
        .expect("fixture project");
    let owner = UserRepository::new(db.clone())
        .upsert_from_github(701_001, "readiness-flow-owner", None, None)
        .await
        .expect("fixture owner");
    (project.id, owner.id)
}

async fn seed_snapshot(db: &Database, project_id: &str, commit_sha: &str) {
    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id,
            commit_sha,
            graph_blob: b"persisted fixture graph",
        })
        .await
        .expect("persist immutable snapshot");
}

fn assert_float_eq(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < f64::EPSILON,
        "expected {expected}, got {actual}"
    );
}
