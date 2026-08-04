//! Postgres conformance coverage for the public TribunalEvidenceReturnV1 repository API.

use djinn_db::{Database, TypedEvidenceRepository};
use serde_json::{Value, json};

struct Attempt {
    finding_id: String,
    task_id: String,
    attempt_id: String,
}

async fn attempt(methods: &[&str]) -> (Database, Attempt) {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let project = uuid::Uuid::now_v7().to_string();
    let user = uuid::Uuid::now_v7().to_string();
    let task = uuid::Uuid::now_v7().to_string();
    let proposal = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO projects (id,name,path,verification_rules) VALUES ($1,$2,$3,'[]')")
        .bind(&project)
        .bind(format!("p-{project}"))
        .bind(format!("/p-{project}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id,github_id,github_login) VALUES ($1,$2,$3)")
        .bind(&user)
        .bind((uuid::Uuid::now_v7().as_u128() & i64::MAX as u128) as i64)
        .bind(format!("u-{user}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,'evidence','','','[]','[]','[]',$4)")
        .bind(&task).bind(&project).bind(task.replace('-', "")).bind(&user).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'evidence','','markdown','[]','draft',1)")
        .bind(&proposal).bind(proposal.replace('-', "")).execute(db.pool()).await.unwrap();
    let finding = uuid::Uuid::now_v7().to_string();
    let attempt_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'spike_active','{}',1,$4)")
        .bind(&finding).bind(&proposal).bind(format!("h-{finding}")).bind(&task).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)")
        .bind(&attempt_id).bind(&finding).bind(&task).execute(db.pool()).await.unwrap();
    for (ordinal, method) in methods.iter().enumerate() {
        sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method) VALUES ($1,$2,$3,$4,$5)")
            .bind(uuid::Uuid::now_v7().to_string()).bind(&attempt_id).bind(ordinal as i32 + 1).bind(*method).bind(*method).execute(db.pool()).await.unwrap();
    }
    (
        db,
        Attempt {
            finding_id: finding,
            task_id: task,
            attempt_id,
        },
    )
}

fn payload(a: &Attempt, checks: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({"version":"TribunalEvidenceReturnV1","finding_id":a.finding_id,"spike_task_id":a.task_id,"attempt_id":a.attempt_id,"conclusion":"evidence","checks":checks})).unwrap()
}

fn check(id: &str, method: &str, status: &str) -> Value {
    let mut value = json!({"check_id":id,"method":method,"status":status,"anchors":[]});
    if status != "passed" {
        value["detail"] = json!("not available");
    }
    value
}

#[tokio::test]
async fn tribunal_evidence_return_v1_persists_methods_statuses_and_replays_once() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/tribunal_evidence_return_v1.json")).unwrap();
    assert_eq!(fixture["version"], "TribunalEvidenceReturnV1");
    assert_eq!(
        fixture["valid_cases"][0]["checks"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(fixture["boundaries"].as_array().unwrap().len(), 8);
    let (db, a) = attempt(&["code", "graph", "command"]).await;
    let mut checks = vec![
        check("code", "code", "passed"),
        check("graph", "graph", "failed"),
        check("command", "command", "not_run"),
    ];
    checks[1]["anchors"] = fixture["valid_cases"][0]["checks"][1]["anchors"].clone();
    checks[2]["anchors"] = fixture["valid_cases"][0]["checks"][2]["anchors"].clone();
    let mut body: Value = serde_json::from_slice(&payload(&a, checks)).unwrap();
    body["failures"] = json!([{"check_id":"graph","code":"failed","detail":"failed"}]);
    body["gaps"] = json!([{"check_id":"command","code":"unavailable","detail":"unavailable"}]);
    let bytes = serde_json::to_vec(&body).unwrap();
    let repo = TypedEvidenceRepository::new(db.clone());
    let first = repo.submit_return_v1(&bytes).await.unwrap();
    assert!(!first.replayed);
    let replay = repo.submit_return_v1(&bytes).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(first.validation_id, replay.validation_id);
    for (table, expected) in [
        ("typed_evidence_check_results", 3_i64),
        ("typed_evidence_issues", 2),
        ("typed_evidence_transitions", 1),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, expected, "{table} is normalized exactly once");
    }
    let lifecycle: String =
        sqlx::query_scalar("SELECT lifecycle FROM typed_evidence_findings WHERE id=$1")
            .bind(&a.finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(lifecycle, "evidence_received");
    let mut changed: Value = serde_json::from_slice(&bytes).unwrap();
    changed["conclusion"] = json!("different");
    assert!(
        repo.submit_return_v1(&serde_json::to_vec(&changed).unwrap())
            .await
            .unwrap_err()
            .to_string()
            .contains("replay_payload_conflict")
    );
}

#[tokio::test]
async fn tribunal_evidence_return_v1_rejects_boundaries_and_marks_malformed_attempt_failed() {
    let (db, a) = attempt(&["code"]).await;
    let repo = TypedEvidenceRepository::new(db.clone());
    let malformed = format!(
        r#"{{"attempt_id":"{}","version":"TribunalEvidenceReturnV1"}}"#,
        a.attempt_id
    );
    assert!(
        repo.submit_return_v1(malformed.as_bytes())
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid_json")
    );
    let failed: String =
        sqlx::query_scalar("SELECT lifecycle FROM typed_evidence_findings WHERE id=$1")
            .bind(&a.finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(failed, "failed");
    let transitions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM typed_evidence_transitions WHERE finding_id=$1")
            .bind(&a.finding_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(transitions, 1);
    let (db, a) = attempt(&["code"]).await;
    let repo = TypedEvidenceRepository::new(db.clone());
    for (field, value, error) in [
        ("conclusion", "x".repeat(8193), "conclusion_too_large"),
        ("finding_id", "x".repeat(2049), "string_too_large"),
    ] {
        let mut body: Value =
            serde_json::from_slice(&payload(&a, vec![check("code", "code", "passed")])).unwrap();
        body[field] = json!(value);
        assert!(
            repo.submit_return_v1(&serde_json::to_vec(&body).unwrap())
                .await
                .unwrap_err()
                .to_string()
                .contains(error)
        );
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM typed_evidence_validation_results")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(rows, 0, "rejection rolls normalized rows back");
    }
    assert!(
        repo.submit_return_v1(&vec![b'x'; 262145])
            .await
            .unwrap_err()
            .to_string()
            .contains("payload_too_large")
    );
}
