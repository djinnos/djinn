//! Executable TribunalEvidenceReturnV1 conformance fixtures.
use djinn_db::{Database, TypedEvidenceRepository};
use serde_json::{Value, json};

struct A {
    finding: String,
    task: String,
    attempt: String,
    invocation: Option<String>,
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
    sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs,created_by_user_id) VALUES ($1,$2,$3,'x','','','[]','[]','[]',$4)").bind(&t).bind(&p).bind(t.replace('-' ,"")).bind(&u).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO proposals (id,short_id,title,body,body_format,acceptance_criteria,status,latest_revision_seq) VALUES ($1,$2,'x','','markdown','[]','draft',1)").bind(&q).bind(q.replace('-' ,"")).execute(db.pool()).await.unwrap();
    let f = uuid::Uuid::now_v7().to_string();
    let a = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO typed_evidence_findings (id,proposal_id,demand_hash,lifecycle,claim,demanded_revision_seq,created_by_task_id) VALUES ($1,$2,$3,'spike_active','{}',1,$4)").bind(&f).bind(&q).bind(format!("h{f}")).bind(&t).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO typed_evidence_attempts (id,finding_id,sequence,spike_task_id) VALUES ($1,$2,1,$3)").bind(&a).bind(&f).bind(&t).execute(db.pool()).await.unwrap();
    for (n, m) in methods.iter().enumerate() {
        sqlx::query("INSERT INTO typed_evidence_planned_checks (id,attempt_id,ordinal,check_id,method) VALUES ($1,$2,$3,$4,$5)").bind(uuid::Uuid::now_v7().to_string()).bind(&a).bind(n as i32+1).bind(*m).bind(*m).execute(db.pool()).await.unwrap();
    }
    let invocation = if methods.contains(&"command") {
        Some(seed_command(&db, &p, &t, &a, 0).await)
    } else {
        None
    };
    (
        db,
        A {
            finding: f,
            task: t,
            attempt: a,
            invocation,
        },
    )
}
async fn seed_command(db: &Database, p: &str, t: &str, a: &str, exit: i32) -> String {
    let s = uuid::Uuid::now_v7().to_string();
    let plan = uuid::Uuid::now_v7().to_string();
    let i = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO sessions (id,project_id,task_id,model_id,agent_type,status) VALUES ($1,$2,$3,'m','worker','running')").bind(&s).bind(p).bind(t).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO evidence_plans (id,spike_task_id,session_id,captured_commit_sha,worktree_fingerprint) VALUES ($1,$2,$3,'abcdef0123456789','w')").bind(&plan).bind(t).bind(&s).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO evidence_plan_checks (plan_id,ordinal,check_id,question,method) VALUES ($1,1,'command','q','command')").bind(&plan).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE typed_evidence_planned_checks SET evidence_plan_id=$1,evidence_plan_check_id='command' WHERE attempt_id=$2 AND check_id='command'").bind(&plan).bind(a).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO evidence_command_invocations (id,plan_id,spike_task_id,session_id,captured_commit_sha,worktree_fingerprint,check_id,argv,canonical_cwd,launch_state,process_state,exit_code,timed_out) VALUES ($1,$2,$3,$4,'abcdef0123456789','w','command','[\"true\"]','/repo','launched','exited',$5,FALSE)").bind(&i).bind(&plan).bind(t).bind(&s).bind(exit).execute(db.pool()).await.unwrap();
    i
}
fn wire(v: &Value, a: &A) -> Vec<u8> {
    serde_json::to_string(v)
        .unwrap()
        .replace("$finding_id", &a.finding)
        .replace("$spike_task_id", &a.task)
        .replace("$attempt_id", &a.attempt)
        .replace(
            "$invocation_id",
            a.invocation.as_deref().unwrap_or("missing"),
        )
        .into_bytes()
}
fn methods(v: &Value) -> Vec<&str> {
    v["planned_methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect()
}
async fn rows(db: &Database, t: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {t}"))
        .fetch_one(db.pool())
        .await
        .unwrap()
}
fn check(id: &str, m: &str, s: &str) -> Value {
    let mut v = json!({"check_id":id,"method":m,"status":s,"anchors":[]});
    if s != "passed" {
        v["detail"] = json!("d")
    }
    v
}
fn payload(a: &A, c: Value) -> Value {
    json!({"version":"TribunalEvidenceReturnV1","finding_id":a.finding,"spike_task_id":a.task,"attempt_id":a.attempt,"conclusion":"x","checks":[c]})
}

#[tokio::test]
async fn tribunal_evidence_return_v1_every_fixture_is_submitted_and_normalized() {
    let fx: Value =
        serde_json::from_str(include_str!("fixtures/tribunal_evidence_return_v1.json")).unwrap();
    for c in fx["invalid_cases"].as_array().unwrap() {
        let (db, a) = setup(&methods(c)).await;
        let e = TypedEvidenceRepository::new(db.clone())
            .submit_return_v1(&wire(&c["payload"], &a))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(c["error"].as_str().unwrap()),
            "{}: {e}",
            c["name"]
        );
        assert_eq!(rows(&db, "typed_evidence_validation_results").await, 0);
    }
    for c in fx["valid_cases"].as_array().unwrap() {
        let (db, a) = setup(&methods(c)).await;
        let w = wire(&c["payload"], &a);
        let r = TypedEvidenceRepository::new(db.clone());
        let got = r.submit_return_v1(&w).await.unwrap();
        assert_eq!(
            format!("{:?}", got.outcome).to_lowercase(),
            c["expected_outcome"].as_str().unwrap()
        );
        assert!(r.submit_return_v1(&w).await.unwrap().replayed);
        let p = &c["payload"];
        assert_eq!(
            rows(&db, "typed_evidence_check_results").await,
            p["checks"].as_array().unwrap().len() as i64
        );
        assert_eq!(
            rows(&db, "typed_evidence_return_findings").await,
            p["findings"].as_array().map_or(0, Vec::len) as i64
        );
        assert_eq!(
            rows(&db, "typed_evidence_issues").await,
            p["failures"].as_array().map_or(0, Vec::len) as i64
                + p["gaps"].as_array().map_or(0, Vec::len) as i64
        );
        let expected_anchors = p["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["anchors"].as_array().map_or(0, Vec::len))
            .sum::<usize>()
            + p["findings"].as_array().map_or(0, |findings| {
                findings
                    .iter()
                    .map(|finding| finding["anchors"].as_array().map_or(0, Vec::len))
                    .sum()
            });
        assert_eq!(
            rows(&db, "typed_evidence_anchors").await
                + rows(&db, "typed_evidence_return_finding_anchors").await,
            expected_anchors as i64
        );
        let outcome: String =
            sqlx::query_scalar("SELECT outcome FROM typed_evidence_validation_results")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(outcome, c["expected_outcome"].as_str().unwrap());
        if c["name"] == "unresolved_all_anchor_families" {
            let h: Vec<String> =
                sqlx::query_scalar("SELECT health FROM typed_evidence_anchor_health")
                    .fetch_all(db.pool())
                    .await
                    .unwrap();
            assert_eq!(h.len(), 7);
            assert!(h.iter().all(|x| x == "unusable"));
        }
    }
}
#[tokio::test]
async fn tribunal_evidence_return_v1_exercises_every_method_and_status_rule() {
    for m in ["code", "graph", "command"] {
        for s in ["passed", "failed", "not_run"] {
            let (db, a) = setup(&[m]).await;
            let mut p = payload(&a, check(m, m, s));
            if m == "command" && s == "passed" {
                p["checks"][0]["invocation_id"] = json!(a.invocation.unwrap())
            }
            if s == "failed" {
                p["failures"] = json!([{"check_id":m,"code":"f","detail":"d"}])
            }
            if s == "not_run" {
                p["gaps"] = json!([{"check_id":m,"code":"g","detail":"d"}])
            }
            TypedEvidenceRepository::new(db.clone())
                .submit_return_v1(&serde_json::to_vec(&p).unwrap())
                .await
                .unwrap();
            assert_eq!(rows(&db, "typed_evidence_check_results").await, 1);
            assert_eq!(
                rows(&db, "typed_evidence_issues").await,
                i64::from(s != "passed")
            );
        }
    }
}
#[tokio::test]
async fn tribunal_evidence_return_v1_unhealthy_owned_command_is_unusable() {
    let (db, a) = setup(&["command"]).await;
    let p: String = sqlx::query_scalar("SELECT project_id FROM tasks WHERE id=$1")
        .bind(&a.task)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let bad = seed_command(&db, &p, &a.task, &a.attempt, 1).await;
    let mut v = payload(&a, check("command", "command", "passed"));
    v["checks"][0]["invocation_id"] = json!(&bad);
    v["checks"][0]["anchors"] = json!([{"method":"command","locator":format!("command:{bad}")}]);
    let got = TypedEvidenceRepository::new(db.clone())
        .submit_return_v1(&serde_json::to_vec(&v).unwrap())
        .await
        .unwrap();
    assert_eq!(format!("{:?}", got.outcome), "Unresolved");
    let usable: bool =
        sqlx::query_scalar("SELECT usable FROM typed_evidence_invocation_provenance")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let health: String = sqlx::query_scalar("SELECT health FROM typed_evidence_anchor_health")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!usable);
    assert_eq!(health, "unusable");
}
