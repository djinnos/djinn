//! Real-Postgres end-to-end readiness service flow.
//!
//! Consumer entry points remain the kickoff and query services. Repository calls
//! below model the correlated Architect callbacks that those consumer surfaces do
//! not own.

use async_trait::async_trait;
use djinn_control_plane::{
    readiness_kickoff::{
        READINESS_SKILL_NAME, READINESS_SKILL_VERSION, ReadinessKickoffRequest,
        ReadinessKickoffService, ReadinessSkillPinError, ReadinessSkillPinResolver,
    },
    readiness_query::{ReadinessProjectQuery, ReadinessQueryService, ReadinessRunQuery},
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
struct AvailablePin;

#[async_trait]
impl ReadinessSkillPinResolver for AvailablePin {
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
async fn two_area_service_flow_persists_worked_terminal_readiness_detail() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id) = seed_owned_project_with_snapshot(&db).await;

    let kickoff = ReadinessKickoffService::new(db.clone(), AvailablePin)
        .kickoff(ReadinessKickoffRequest {
            project_id: project_id.clone(),
            authenticated_owner_id: owner_id.clone(),
            idempotency_key: "two-area-service-flow".into(),
        })
        .await
        .expect("owner kickoff with persisted snapshot");
    assert_eq!(kickoff.run.repository_snapshot, SNAPSHOT);
    assert_eq!(kickoff.run.skill_name, READINESS_SKILL_NAME);
    assert_eq!(kickoff.run.skill_version, READINESS_SKILL_VERSION);

    let repository = ReadinessRepository::new(db.clone());
    let fanout = repository
        .complete_identification(&kickoff.run.id, &owner_id, two_area_identification())
        .await
        .expect("freeze and fan out two identified areas");
    assert_eq!(fanout.len(), 2);

    let frontend = fanout
        .iter()
        .find(|item| item.area.area_key == "frontend")
        .expect("TypeScript frontend fanout");
    let backend = fanout
        .iter()
        .find(|item| item.area.area_key == "backend")
        .expect("Rust backend fanout");
    assert_eq!(frontend.attempt.attempt_number, 1);
    assert_eq!(backend.attempt.attempt_number, 1);

    assert_eq!(
        repository
            .ingest_area_result(callback(
                &kickoff.run.id,
                frontend,
                frontend_result(&frontend.area.id),
            ))
            .await
            .expect("accept TypeScript callback"),
        ReadinessCallbackOutcome::Accepted
    );
    assert_eq!(
        repository
            .ingest_area_result(callback(
                &kickoff.run.id,
                backend,
                backend_result(&backend.area.id),
            ))
            .await
            .expect("accept Rust callback"),
        ReadinessCallbackOutcome::Accepted
    );

    let aggregation = repository
        .aggregate_run(&kickoff.run.id, "readiness-service-flow-aggregator")
        .await
        .expect("terminalize complete run");
    assert_eq!(aggregation.status, "completed_with_errors");
    assert_eq!(aggregation.area_scores.len(), 2);
    assert_close(aggregation.project_score.score, 7.0 / 9.0);
    assert_eq!(aggregation.project_score.band, "ready");

    let detail = ReadinessQueryService::new(db)
        .run_detail(ReadinessRunQuery {
            project_id,
            run_id: kickoff.run.id.clone(),
            authenticated_owner_id: owner_id,
        })
        .await
        .expect("read terminal detail only through service boundary");

    assert_eq!(detail.run.id, kickoff.run.id);
    assert_eq!(detail.run.status, "completed_with_errors");
    assert_eq!(detail.run.expected_area_count, Some(2));
    assert_eq!(detail.areas.len(), 2);

    let frontend_detail = detail
        .areas
        .iter()
        .find(|area| area.area_key == "frontend")
        .expect("frozen frontend projection");
    assert_eq!(
        frontend_detail.composition["languages"],
        serde_json::json!(["TypeScript"])
    );
    assert_eq!(
        frontend_detail.composition["roles"],
        serde_json::json!(["frontend"])
    );
    assert_eq!(frontend_detail.path_scopes, serde_json::json!(["web/"]));
    assert_current_success(frontend_detail);
    assert_finding(
        frontend_detail,
        "frontend-auth",
        "covered",
        "high",
        0.95,
        serde_json::json!([{"path":"web/auth.ts","line":12}]),
    );
    assert_finding(
        frontend_detail,
        "frontend-inputs",
        "partial",
        "medium",
        0.80,
        serde_json::json!([{"path":"web/forms.ts","line":31}]),
    );
    assert_eq!(frontend_detail.accepted_outputs.len(), 1);
    assert_eq!(
        frontend_detail.accepted_outputs[0].result,
        frontend_result(&frontend.area.id)
    );
    assert_eq!(
        frontend_detail.accepted_outputs[0].result["warnings"][0]["reason"],
        "legacy form remains outside migration scope"
    );

    let backend_detail = detail
        .areas
        .iter()
        .find(|area| area.area_key == "backend")
        .expect("frozen backend projection");
    assert_eq!(
        backend_detail.composition["languages"],
        serde_json::json!(["Rust"])
    );
    assert_eq!(
        backend_detail.composition["roles"],
        serde_json::json!(["backend"])
    );
    assert_eq!(backend_detail.path_scopes, serde_json::json!(["server/"]));
    assert_current_success(backend_detail);
    assert_finding(
        backend_detail,
        "backend-auth",
        "covered",
        "high",
        0.90,
        serde_json::json!([{"path":"server/src/auth.rs","line":48}]),
    );
    assert_finding(
        backend_detail,
        "backend-secrets",
        "analysis_error",
        "low",
        0.85,
        serde_json::json!([{"path":"server/src/config.rs","line":9}]),
    );
    assert_eq!(backend_detail.accepted_outputs.len(), 1);
    assert_eq!(
        backend_detail.accepted_outputs[0].result,
        backend_result(&backend.area.id)
    );
    assert_eq!(
        backend_detail.accepted_outputs[0].result["warnings"][0]["reason"],
        "secret rotation is not configured"
    );

    // Worked arithmetic: frontend=(3 + 2 * 0.5)/(3 + 2)=4/5;
    // backend=(3 + 1 * 0)/(3 + 1)=3/4; project=(4 + 3)/(5 + 4)=7/9.
    let frontend_score = detail
        .area_scores
        .iter()
        .find(|score| score.area_id == frontend_detail.id)
        .expect("frontend score");
    assert_close(frontend_score.score, 4.0 / 5.0);
    assert_eq!(frontend_score.applicable_weight, 5);
    assert_close(frontend_score.covered_weight, 4.0);
    assert_eq!(frontend_score.status, "supported");
    let backend_score = detail
        .area_scores
        .iter()
        .find(|score| score.area_id == backend_detail.id)
        .expect("backend score");
    assert_close(backend_score.score, 3.0 / 4.0);
    assert_eq!(backend_score.applicable_weight, 4);
    assert_close(backend_score.covered_weight, 3.0);
    assert_eq!(backend_score.status, "supported");
    let project_score = detail.project_score.expect("terminal project score");
    assert_close(project_score.score, 7.0 / 9.0);
    assert_eq!(project_score.band, "ready");

    assert_eq!(detail.suggestions.len(), 1);
    let suggestion = &detail.suggestions[0];
    assert_eq!(suggestion.dedupe_key, "shared-auth-remediation");
    let mut contributing_area_ids = vec![backend.area.id.clone(), frontend.area.id.clone()];
    contributing_area_ids.sort();
    assert_eq!(
        suggestion.suggestion,
        serde_json::json!({
            "dedupe_key": "shared-auth-remediation",
            "action": "Apply shared authentication remediation",
            "area_ids": contributing_area_ids,
            "guardrail_ids": ["backend-auth", "frontend-auth"]
        })
    );
}

#[tokio::test]
async fn library_only_unsupported_app_guardrails_do_not_penalize_readiness() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id) = seed_owned_project_with_snapshot(&db).await;
    let kickoff = ReadinessKickoffService::new(db.clone(), AvailablePin)
        .kickoff(ReadinessKickoffRequest {
            project_id: project_id.clone(),
            authenticated_owner_id: owner_id.clone(),
            idempotency_key: "library-only-unsupported-guardrails".into(),
        })
        .await
        .expect("owner kickoff with persisted snapshot");

    let repository = ReadinessRepository::new(db.clone());
    let fanout = repository
        .complete_identification(&kickoff.run.id, &owner_id, library_identification())
        .await
        .expect("freeze and fan out the library area");
    assert_eq!(fanout.len(), 1);
    let library = fanout.first().expect("library fanout");
    assert_eq!(library.area.area_key, "library");
    assert_eq!(library.attempt.attempt_number, 1);
    assert_eq!(
        repository
            .ingest_area_result(callback(&kickoff.run.id, library, library_result()))
            .await
            .expect("accept correlated library callback"),
        ReadinessCallbackOutcome::Accepted
    );

    let aggregation = repository
        .aggregate_run(&kickoff.run.id, "library-only-service-flow-aggregator")
        .await
        .expect("terminalize complete library run");
    assert_eq!(aggregation.status, "completed");
    assert_eq!(aggregation.area_scores.len(), 1);
    assert_close(aggregation.project_score.score, 1.0);
    assert_eq!(aggregation.project_score.band, "strong");

    let detail = ReadinessQueryService::new(db)
        .run_detail(ReadinessRunQuery {
            project_id,
            run_id: kickoff.run.id.clone(),
            authenticated_owner_id: owner_id,
        })
        .await
        .expect("read library detail only through service boundary");
    assert_eq!(detail.run.id, kickoff.run.id);
    assert_eq!(detail.run.status, "completed");
    assert_eq!(detail.run.expected_area_count, Some(1));
    assert_eq!(detail.areas.len(), 1);
    let library_detail = &detail.areas[0];
    assert_eq!(library_detail.area_key, "library");
    assert_eq!(
        library_detail.composition["roles"],
        serde_json::json!(["library"])
    );
    assert_eq!(
        library_detail.composition["key_libraries"],
        serde_json::json!(["serde"])
    );
    assert_eq!(
        library_detail.path_scopes,
        serde_json::json!(["crates/sdk/"])
    );
    assert_current_success(library_detail);
    assert_finding(
        library_detail,
        "library-public-api",
        "covered",
        "critical",
        0.96,
        serde_json::json!([{"path":"crates/sdk/src/lib.rs","line":18}]),
    );
    assert_finding(
        library_detail,
        "app-session-auth",
        "unsupported",
        "critical",
        1.0,
        serde_json::json!([{"path":"crates/sdk/src/lib.rs","line":1}]),
    );
    assert_eq!(library_detail.accepted_outputs.len(), 1);
    assert_eq!(
        library_detail.accepted_outputs[0].result["unsupported"],
        serde_json::json!([{
            "guardrail_key": "app-session-auth",
            "reason": "session authentication applies to applications, not this library",
            "evidence": [{"path":"crates/sdk/src/lib.rs","line":1}]
        }])
    );

    // Worked arithmetic: public API coverage=5/5. The app-only critical
    // guardrail is explicitly unsupported, so it contributes neither its five
    // points to the numerator nor to the denominator: 5/5, not 5/10.
    let library_score = detail
        .area_scores
        .iter()
        .find(|score| score.area_id == library_detail.id)
        .expect("library score");
    assert_close(library_score.score, 1.0);
    assert_eq!(library_score.applicable_weight, 5);
    assert_close(library_score.covered_weight, 5.0);
    assert_eq!(library_score.status, "supported");
    let project_score = detail.project_score.expect("terminal project score");
    assert_close(project_score.score, 1.0);
    assert_eq!(project_score.band, "strong");
}

#[tokio::test]
async fn terminal_run_rerun_uses_new_context_without_mutating_prior_detail() {
    let db = Database::ephemeral().await.expect("real postgres database");
    let (project_id, owner_id) = seed_owned_project_with_snapshot(&db).await;
    let kickoff_service = ReadinessKickoffService::new(db.clone(), AvailablePin);
    let query_service = ReadinessQueryService::new(db.clone());

    let first = kickoff_service
        .kickoff(ReadinessKickoffRequest {
            project_id: project_id.clone(),
            authenticated_owner_id: owner_id.clone(),
            idempotency_key: "terminal-run".into(),
        })
        .await
        .expect("start first run against the initial immutable context");
    let repository = ReadinessRepository::new(db.clone());
    let fanout = repository
        .complete_identification(&first.run.id, &owner_id, library_identification())
        .await
        .expect("complete first-run identification");
    let library = fanout.first().expect("first-run library work");
    assert_eq!(
        repository
            .ingest_area_result(callback(&first.run.id, library, library_result()))
            .await
            .expect("accept first-run callback"),
        ReadinessCallbackOutcome::Accepted
    );
    assert_eq!(
        repository
            .aggregate_run(&first.run.id, "terminal-rerun-aggregator")
            .await
            .expect("terminalize first run")
            .status,
        "completed"
    );

    let first_detail = query_service
        .run_detail(ReadinessRunQuery {
            project_id: project_id.clone(),
            run_id: first.run.id.clone(),
            authenticated_owner_id: owner_id.clone(),
        })
        .await
        .expect("capture complete first terminal detail through service boundary");

    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id: &project_id,
            commit_sha: RERUN_SNAPSHOT,
            graph_blob: b"new persisted service-flow graph",
        })
        .await
        .expect("persist newer immutable repository snapshot");

    let second = kickoff_service
        .kickoff(ReadinessKickoffRequest {
            project_id: project_id.clone(),
            authenticated_owner_id: owner_id.clone(),
            idempotency_key: "terminal-rerun".into(),
        })
        .await
        .expect("start a distinct run after the first reaches terminal state");
    assert_ne!(second.run.id, first.run.id);
    assert_ne!(second.identification_task.id, first.identification_task.id);
    assert_eq!(second.run.status, "identifying");
    assert_eq!(second.run.repository_snapshot, RERUN_SNAPSHOT);
    assert_eq!(second.run.skill_name, READINESS_SKILL_NAME);
    assert_eq!(second.run.skill_version, READINESS_SKILL_VERSION);

    let second_task_context: serde_json::Value =
        serde_json::from_str(&second.identification_task.description)
            .expect("identification task has structured immutable context");
    assert_eq!(second_task_context["run_id"], second.run.id);
    assert_eq!(second_task_context["repository_snapshot"], RERUN_SNAPSHOT);
    assert_eq!(second_task_context["skill_name"], READINESS_SKILL_NAME);
    assert_eq!(
        second_task_context["skill_version"],
        READINESS_SKILL_VERSION
    );

    let redelivery = kickoff_service
        .kickoff(ReadinessKickoffRequest {
            project_id: project_id.clone(),
            authenticated_owner_id: owner_id.clone(),
            idempotency_key: "terminal-rerun".into(),
        })
        .await
        .expect("redeliver the second kickoff key");
    assert_eq!(redelivery.run.id, second.run.id);
    assert_eq!(
        redelivery.identification_task.id, second.identification_task.id,
        "same-key redelivery must not create duplicate identification work"
    );
    assert_eq!(
        query_service
            .active_or_latest(ReadinessProjectQuery {
                project_id: project_id.clone(),
                authenticated_owner_id: owner_id.clone(),
            })
            .await
            .expect("read active rerun through service boundary")
            .expect("second run remains active")
            .id,
        second.run.id
    );

    assert_eq!(
        query_service
            .run_detail(ReadinessRunQuery {
                project_id,
                run_id: first.run.id,
                authenticated_owner_id: owner_id,
            })
            .await
            .expect("reread first terminal detail through service boundary"),
        first_detail,
        "a later kickoff must not mutate any persisted first-run detail"
    );
}

const SNAPSHOT: &str = "d34db33fd34db33fd34db33fd34db33fd34db33f";
const RERUN_SNAPSHOT: &str = "f00dbabef00dbabef00dbabef00dbabef00dbabe";

async fn seed_owned_project_with_snapshot(db: &Database) -> (String, String) {
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create(
            "readiness-service-flow",
            "readiness-flow-owner",
            "service-flow",
        )
        .await
        .expect("create project");
    let owner = UserRepository::new(db.clone())
        .upsert_from_github(603_001, "readiness-flow-owner", None, None)
        .await
        .expect("create owner");
    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: SNAPSHOT,
            graph_blob: b"persisted service-flow graph",
        })
        .await
        .expect("persist immutable repository snapshot");
    (project.id, owner.id)
}

fn two_area_identification() -> ReadinessIdentificationOutput {
    ReadinessIdentificationOutput {
        areas: vec![
            ReadinessIdentifiedArea {
                area_key: "frontend".into(),
                path_scopes: vec!["web/".into()],
                languages: vec!["TypeScript".into()],
                roles: vec!["frontend".into()],
                frameworks: vec!["React".into()],
                key_libraries: vec!["zod".into()],
                confidence: 0.94,
                evidence: vec!["web/package.json".into()],
            },
            ReadinessIdentifiedArea {
                area_key: "backend".into(),
                path_scopes: vec!["server/".into()],
                languages: vec!["Rust".into()],
                roles: vec!["backend".into()],
                frameworks: vec!["Axum".into()],
                key_libraries: vec!["sqlx".into()],
                confidence: 0.97,
                evidence: vec!["server/Cargo.toml".into()],
            },
        ],
    }
}

fn library_identification() -> ReadinessIdentificationOutput {
    ReadinessIdentificationOutput {
        areas: vec![ReadinessIdentifiedArea {
            area_key: "library".into(),
            path_scopes: vec!["crates/sdk/".into()],
            languages: vec!["Rust".into()],
            roles: vec!["library".into()],
            frameworks: vec![],
            key_libraries: vec!["serde".into()],
            confidence: 0.98,
            evidence: vec!["crates/sdk/Cargo.toml".into()],
        }],
    }
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

fn frontend_result(area_id: &str) -> serde_json::Value {
    serde_json::json!({
        "findings": [
            {"guardrail_key":"frontend-auth","status":"covered","severity":"high","confidence":0.95,"evidence":[{"path":"web/auth.ts","line":12}]},
            {"guardrail_key":"frontend-inputs","status":"partial","severity":"medium","confidence":0.80,"evidence":[{"path":"web/forms.ts","line":31}]}
        ],
        "unsupported": [],
        "warnings": [{"reason":"legacy form remains outside migration scope"}],
        "remediation_suggestions": [{
            "dedupe_key": "shared-auth-remediation",
            "action": "Configure shared authentication remediation",
            "area_id": area_id,
            "guardrail_id": "frontend-auth"
        }]
    })
}

fn backend_result(area_id: &str) -> serde_json::Value {
    serde_json::json!({
        "findings": [
            {"guardrail_key":"backend-auth","status":"covered","severity":"high","confidence":0.90,"evidence":[{"path":"server/src/auth.rs","line":48}]},
            {"guardrail_key":"backend-secrets","status":"analysis_error","severity":"low","confidence":0.85,"evidence":[{"path":"server/src/config.rs","line":9}]}
        ],
        "unsupported": [],
        "warnings": [{"reason":"secret rotation is not configured"}],
        "remediation_suggestions": [{
            "action": "Apply shared authentication remediation",
            "dedupe_key": "shared-auth-remediation",
            "area_ids": [area_id, area_id],
            "guardrail_ids": ["frontend-auth", "backend-auth", "backend-auth"]
        }]
    })
}

fn library_result() -> serde_json::Value {
    serde_json::json!({
        "findings": [
            {"guardrail_key":"library-public-api","status":"covered","severity":"critical","confidence":0.96,"evidence":[{"path":"crates/sdk/src/lib.rs","line":18}]},
            {"guardrail_key":"app-session-auth","status":"unsupported","severity":"critical","confidence":1.0,"evidence":[{"path":"crates/sdk/src/lib.rs","line":1}]}
        ],
        "unsupported": [{
            "guardrail_key": "app-session-auth",
            "reason": "session authentication applies to applications, not this library",
            "evidence": [{"path":"crates/sdk/src/lib.rs","line":1}]
        }],
        "warnings": [],
        "remediation_suggestions": []
    })
}

fn assert_current_success(area: &djinn_control_plane::readiness_query::ReadinessAreaDto) {
    assert_eq!(area.status, "succeeded");
    assert_eq!(area.attempts.len(), 1);
    assert_eq!(area.attempts[0].attempt_number, 1);
    assert_eq!(area.attempts[0].status, "succeeded");
    assert!(area.attempts[0].is_current);
    assert!(area.attempts[0].payload_digest.is_some());
}

fn assert_finding(
    area: &djinn_control_plane::readiness_query::ReadinessAreaDto,
    guardrail_key: &str,
    status: &str,
    severity: &str,
    confidence: f64,
    evidence: serde_json::Value,
) {
    let finding = area
        .accepted_findings
        .iter()
        .find(|finding| finding.guardrail_key == guardrail_key)
        .expect("accepted finding");
    assert_eq!(finding.status, status);
    assert_eq!(finding.severity, severity);
    assert_close(finding.confidence, confidence);
    assert_eq!(finding.evidence, evidence);
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < f64::EPSILON,
        "expected {expected}, got {actual}"
    );
}
