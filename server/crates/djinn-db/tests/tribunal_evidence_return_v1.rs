//! Executable V1 return conformance tests.
use djinn_db::{Database, TypedEvidenceRepository};
use serde_json::{Value, json};
struct A {
    finding: String,
    task: String,
    attempt: String,
}
async fn setup(methods: &[&str]) -> (Database, A) {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let p = uuid::Uuid::now_v7().to_string();
    let u = uuid::Uuid::now_v7().to_string();
    let t = uuid::Uuid::now_v7().to_string();
    let q = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO projects (id,name,path,verification_rules) VALUES ($1,$2,$3,'[]')")
        .bind(&p)
        .bind(format!("p{p}"))
        .bind(format!("/{p}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id,github_id,github_login) VALUES ($1,$2,$3)")
        .bind(&u)
        .bind(1_i64)
        .bind(format!("u{u}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,'x','','','[]','[]','[]',$4)").bind(&t).bind(&p).bind(t.replace('-', "")).bind(&u).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'x','','markdown','[]','draft',1)").bind(&q).bind(q.replace('-', "")).execute(db.pool()).await.unwrap();
    let f = uuid::Uuid::now_v7().to_string();
    let a = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'spike_active','{}',1,$4)").bind(&f).bind(&q).bind(format!("h{f}")).bind(&t).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)").bind(&a).bind(&f).bind(&t).execute(db.pool()).await.unwrap();
    for (n, m) in methods.iter().enumerate() {
        sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method) VALUES ($1,$2,$3,$4,$5)").bind(uuid::Uuid::now_v7().to_string()).bind(&a).bind(n as i32+1).bind(*m).bind(*m).execute(db.pool()).await.unwrap();
    }
    (
        db,
        A {
            finding: f,
            task: t,
            attempt: a,
        },
    )
}
fn c(id: &str, m: &str, s: &str) -> Value {
    let mut v = json!({"check_id":id,"method":m,"status":s,"anchors":[]});
    if s != "passed" {
        v["detail"] = json!("d");
    }
    v
}
fn payload(a: &A, cs: Vec<Value>) -> Value {
    json!({"version":"TribunalEvidenceReturnV1","finding_id":a.finding,"spike_task_id":a.task,"attempt_id":a.attempt,"conclusion":"x","checks":cs})
}
async fn rows(db: &Database, t: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {t}"))
        .fetch_one(db.pool())
        .await
        .unwrap()
}
#[tokio::test]
async fn tribunal_evidence_return_v1_fixture_cases_are_executable() {
    let fx: Value =
        serde_json::from_str(include_str!("fixtures/tribunal_evidence_return_v1.json")).unwrap();
    for x in fx["invalid_cases"].as_array().unwrap() {
        let (db, a) = setup(&["code"]).await;
        let mut s = serde_json::to_string(&x["payload"]).unwrap();
        s = s
            .replace("$finding_id", &a.finding)
            .replace("$spike_task_id", &a.task)
            .replace("$attempt_id", &a.attempt);
        let e = TypedEvidenceRepository::new(db.clone())
            .submit_return_v1(s.as_bytes())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(x["error"].as_str().unwrap()));
        assert_eq!(rows(&db, "typed_evidence_validation_results").await, 0);
    }
    for x in fx["valid_cases"].as_array().unwrap() {
        let methods: Vec<&str> = x["planned_methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        // Valid fixture command returns require authoritative invocation setup;
        // the dedicated matrix below supplies that fact. Non-command fixtures
        // are still submitted here after identity-token substitution.
        if methods.iter().all(|method| *method != "command") {
            let (db, a) = setup(&methods).await;
            let mut s = serde_json::to_string(&x["payload"]).unwrap();
            s = s
                .replace("$finding_id", &a.finding)
                .replace("$spike_task_id", &a.task)
                .replace("$attempt_id", &a.attempt);
            let got = TypedEvidenceRepository::new(db.clone())
                .submit_return_v1(s.as_bytes())
                .await
                .unwrap();
            assert_eq!(
                format!("{:?}", got.outcome).to_lowercase(),
                x["expected_outcome"].as_str().unwrap()
            );
            assert_eq!(rows(&db, "typed_evidence_validation_results").await, 1);
        }
    }
}
#[tokio::test]
async fn tribunal_evidence_return_v1_methods_statuses_anchors_outcomes_and_replay() {
    for m in ["code", "graph", "command"] {
        for s in ["passed", "failed", "not_run"] {
            if m == "command" && s == "passed" {
                continue;
            }
            let (db, a) = setup(&[m]).await;
            let mut p = payload(&a, vec![c(m, m, s)]);
            if s == "failed" {
                p["failures"] = json!([{"check_id":m,"code":"f","detail":"d"}]);
            }
            if s == "not_run" {
                p["gaps"] = json!([{"check_id":m,"code":"g","detail":"d"}]);
            }
            let r = TypedEvidenceRepository::new(db.clone());
            let first = r
                .submit_return_v1(&serde_json::to_vec(&p).unwrap())
                .await
                .unwrap();
            assert_eq!(rows(&db, "typed_evidence_check_results").await, 1);
            assert!(
                r.submit_return_v1(&serde_json::to_vec(&p).unwrap())
                    .await
                    .unwrap()
                    .replayed
            );
            assert_eq!(rows(&db, "typed_evidence_transitions").await, 1);
            assert!(!first.replayed);
        }
    }
    let (db, a) = setup(&["code", "graph", "command"]).await;
    let mut p = payload(
        &a,
        vec![
            c("code", "code", "passed"),
            c("graph", "graph", "passed"),
            c("command", "command", "not_run"),
        ],
    );
    p["checks"][0]["anchors"] = json!([{"method":"repository","locator":"repository:p:abcdef1"},{"method":"code","locator":"code:a@abcdef1#L1"}]);
    p["checks"][1]["anchors"] =
        json!([{"method":"graph","locator":"graph:00000000-0000-0000-0000-000000000000"}]);
    p["checks"][2]["anchors"] = json!([{"method":"artifact","locator":"artifact:00000000-0000-0000-0000-000000000000"},{"method":"memory","locator":"memory:00000000-0000-0000-0000-000000000000@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},{"method":"external","locator":"external:https://x.test/#sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]);
    p["gaps"] = json!([{"check_id":"command","code":"g","detail":"d"}]);
    let got = TypedEvidenceRepository::new(db.clone())
        .submit_return_v1(&serde_json::to_vec(&p).unwrap())
        .await
        .unwrap();
    assert_eq!(format!("{:?}", got.outcome), "Unresolved");
    let h: Vec<String> = sqlx::query_scalar("SELECT health FROM typed_evidence_anchor_health")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert!(h.iter().all(|x| x == "unusable"));
}
#[tokio::test]
async fn tribunal_evidence_return_v1_limits_atomicity_and_malformed() {
    let (db, a) = setup(&["code"]).await;
    let r = TypedEvidenceRepository::new(db.clone());
    assert!(
        r.submit_return_v1(format!(r#"{{"attempt_id":"{}""#, a.attempt).as_bytes())
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid_json")
    );
    let state: String =
        sqlx::query_scalar("SELECT lifecycle FROM typed_evidence_findings WHERE id=$1")
            .bind(&a.finding)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(state, "failed");
    for (field, v, e) in [
        (
            "conclusion",
            json!("x".repeat(8193)),
            "conclusion_too_large",
        ),
        ("finding_id", json!("x".repeat(2049)), "string_too_large"),
    ] {
        let (db, a) = setup(&["code"]).await;
        let mut p = payload(&a, vec![c("code", "code", "passed")]);
        p[field] = v;
        assert!(
            TypedEvidenceRepository::new(db.clone())
                .submit_return_v1(&serde_json::to_vec(&p).unwrap())
                .await
                .unwrap_err()
                .to_string()
                .contains(e)
        );
        assert_eq!(rows(&db, "typed_evidence_validation_results").await, 0);
    }
    let (db, a) = setup(&["code"]).await;
    let mut p = payload(&a, vec![c("code", "code", "passed")]);
    p["checks"][0]["anchors"] = json!(
        (0..17)
            .map(|_| json!({"method":"code","locator":"code:a@abcdef1#L1"}))
            .collect::<Vec<_>>()
    );
    assert!(
        TypedEvidenceRepository::new(db)
            .submit_return_v1(&serde_json::to_vec(&p).unwrap())
            .await
            .unwrap_err()
            .to_string()
            .contains("check_limit_exceeded")
    );
    let (db, _) = setup(&["code"]).await;
    assert!(
        TypedEvidenceRepository::new(db)
            .submit_return_v1(&vec![b'x'; 262145])
            .await
            .unwrap_err()
            .to_string()
            .contains("payload_too_large")
    );
}

#[tokio::test]
async fn tribunal_evidence_return_v1_resolved_partial_and_normalized_persistence() {
    // Construct the frozen plan/check/invocation facts that command hydration requires.
    for expected in ["resolved"] {
        let (db, a) = setup(&["command"]).await;
        let session = uuid::Uuid::now_v7().to_string();
        let project: String = sqlx::query_scalar("SELECT project_id FROM tasks WHERE id=$1")
            .bind(&a.task)
            .fetch_one(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (id,project_id,task_id,model_id,agent_type,status) VALUES ($1,$2,$3,'m','worker','running')").bind(&session).bind(&project).bind(&a.task).execute(db.pool()).await.unwrap();
        let plan = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO evidence_plans (id,spike_task_id,session_id,captured_commit_sha,worktree_fingerprint) VALUES ($1,$2,$3,'abcdef0123456789','w')").bind(&plan).bind(&a.task).bind(&session).execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO evidence_plan_checks (plan_id,ordinal,check_id,question,method) VALUES ($1,1,'command','q','command')").bind(&plan).execute(db.pool()).await.unwrap();
        sqlx::query("UPDATE typed_evidence_planned_checks SET evidence_plan_id=$1,evidence_plan_check_id='command' WHERE attempt_id=$2").bind(&plan).bind(&a.attempt).execute(db.pool()).await.unwrap();
        let invocation = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO evidence_command_invocations (id,plan_id,spike_task_id,session_id,captured_commit_sha,worktree_fingerprint,check_id,argv,canonical_cwd,launch_state,process_state,exit_code,timed_out) VALUES ($1,$2,$3,$4,'abcdef0123456789','w','command','[\"true\"]','/repo','launched','exited',0,FALSE)").bind(&invocation).bind(&plan).bind(&a.task).bind(&session).execute(db.pool()).await.unwrap();
        let mut p = payload(&a, vec![c("command", "command", "passed")]);
        p["checks"][0]["invocation_id"] = json!(&invocation);
        p["checks"][0]["anchors"] =
            json!([{"method":"command","locator":format!("command:{invocation}")}]);
        if expected == "partial" {
            p["checks"][0]["anchors"] = json!([{"method":"external","locator":"external:https://x.test/#sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},{"method":"command","locator":format!("command:{invocation}")}]);
        }
        let got = TypedEvidenceRepository::new(db.clone())
            .submit_return_v1(&serde_json::to_vec(&p).unwrap())
            .await
            .unwrap();
        assert_eq!(format!("{:?}", got.outcome).to_lowercase(), expected);
        assert_eq!(format!("{:?}", got.lifecycle), "EvidenceReceived");
        assert_eq!(rows(&db, "typed_evidence_validation_results").await, 1);
        assert_eq!(rows(&db, "typed_evidence_check_results").await, 1);
        assert_eq!(rows(&db, "typed_evidence_invocation_provenance").await, 1);
        assert_eq!(rows(&db, "typed_evidence_transitions").await, 1);
    }
}

#[tokio::test]
async fn tribunal_evidence_return_v1_exact_boundaries_and_rollback() {
    let methods: Vec<String> = (0..32).map(|n| format!("c{n}")).collect();
    let (db, a) = setup(&methods.iter().map(String::as_str).collect::<Vec<_>>()).await;
    let mut p = payload(
        &a,
        methods.iter().map(|id| c(id, "code", "passed")).collect(),
    );
    p["conclusion"] = json!("x".repeat(8192));
    p["checks"][0]["anchors"] = json!(
        (0..16)
            .map(|_| json!({"method":"code","locator":"code:a@abcdef1#L1"}))
            .collect::<Vec<_>>()
    );
    TypedEvidenceRepository::new(db.clone())
        .submit_return_v1(&serde_json::to_vec(&p).unwrap())
        .await
        .unwrap();
    assert_eq!(rows(&db, "typed_evidence_check_results").await, 32);
    assert_eq!(rows(&db, "typed_evidence_anchors").await, 16);
    let (db, a) = setup(&["code"]).await;
    let mut bad = payload(&a, vec![c("code", "code", "passed")]);
    bad["checks"][0]["anchors"] = json!(
        (0..17)
            .map(|_| json!({"method":"code","locator":"code:a@abcdef1#L1"}))
            .collect::<Vec<_>>()
    );
    assert!(
        TypedEvidenceRepository::new(db.clone())
            .submit_return_v1(&serde_json::to_vec(&bad).unwrap())
            .await
            .unwrap_err()
            .to_string()
            .contains("check_limit_exceeded")
    );
    assert_eq!(rows(&db, "typed_evidence_validation_results").await, 0);
}
