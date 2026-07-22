//! Isolated migration 138 coverage for refinement run identity and legacy stop normalization.

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};
use std::path::PathBuf;

const VERSION: u64 = 138;
const FILE: &str = "138_refinement_runs_and_intents.sql";

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}
fn entries() -> Vec<(u64, PathBuf)> {
    let mut v: Vec<_> = std::fs::read_dir(migrations_dir())
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            let n = p.file_name()?.to_str()?;
            Some((n.split_once('_')?.0.parse().ok()?, p))
        })
        .collect();
    v.sort_by_key(|e| e.0);
    v
}
async fn temporary<T, Fut>(name: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = djinn_db::test_database_base_url();
    let prefix = base
        .rsplit_once('/')
        .map(|x| x.0)
        .unwrap_or(&base)
        .trim_end_matches('/');
    let db = format!(
        "djinn_refinement_{}_{}",
        name,
        uuid::Uuid::now_v7().simple()
    );
    let admin = format!("{prefix}/postgres");
    let mut c = PgConnection::connect(&admin).await.unwrap();
    c.execute(format!(r#"CREATE DATABASE "{db}""#).as_str())
        .await
        .unwrap();
    drop(c);
    let result = f(format!("{prefix}/{db}")).await;
    let mut c = PgConnection::connect(&admin).await.unwrap();
    let _ = c.execute(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid()").as_str()).await;
    c.execute(format!(r#"DROP DATABASE "{db}""#).as_str())
        .await
        .unwrap();
    result
}
async fn prior(c: &mut PgConnection) {
    for (v, p) in entries() {
        if v >= VERSION {
            break;
        }
        c.execute(std::fs::read_to_string(p).unwrap().as_str())
            .await
            .unwrap();
    }
}
async fn migration(c: &mut PgConnection) {
    c.execute(
        std::fs::read_to_string(migrations_dir().join(FILE))
            .unwrap()
            .as_str(),
    )
    .await
    .unwrap();
}
async fn proposal(pool: &sqlx::PgPool, id: &str, short: &str) {
    sqlx::query("INSERT INTO proposals (id, short_id, title) VALUES ($1,$2,'proposal')")
        .bind(id)
        .bind(short)
        .execute(pool)
        .await
        .unwrap();
}
async fn lifecycle(
    pool: &sqlx::PgPool,
    id: &str,
    proposal: &str,
    kind: &str,
    at: &str,
    meta: Option<serde_json::Value>,
) {
    sqlx::query("INSERT INTO proposal_revisions (id,proposal_id,seq,title,body,event_kind,event_metadata,created_at) VALUES ($1,$2,1,'','',$3,$4,$5)")
        .bind(id).bind(proposal).bind(kind).bind(meta).bind(at).execute(pool).await.unwrap();
}

#[tokio::test]
async fn migration_138_backfills_deterministic_intervals_and_stop_vocabulary() {
    temporary("history", |url| async move {
        let mut c = PgConnection::connect(&url).await.unwrap(); prior(&mut c).await;
        let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
        proposal(&pool, "p-1", "p001").await; proposal(&pool, "p-2", "p002").await;
        proposal(&pool, "p-overlap", "p003").await; proposal(&pool, "p-solo", "p004").await;
        sqlx::query("INSERT INTO projects (id,name,github_owner,github_repo) VALUES ('project','project','owner','repo')").execute(&pool).await.unwrap();
        for (id, short) in [("task-ambiguous", "ta001"), ("task-solo", "ts001")] {
            sqlx::query("INSERT INTO tasks (id,project_id,short_id,title,description,design,labels,acceptance_criteria,memory_refs) VALUES ($1,'project',$2,'','','','[]'::jsonb,'[]'::jsonb,'[]'::jsonb)").bind(id).bind(short).execute(&pool).await.unwrap();
        }
        lifecycle(&pool,"a-start","p-overlap","refinement_start","2025-01-01T10:00:00Z",None).await;
        lifecycle(&pool,"b-start","p-overlap","refinement_start","2025-01-01T10:01:00Z",None).await;
        lifecycle(&pool,"ambiguous","p-overlap","status_change","2025-01-01T10:02:00Z",None).await;
        sqlx::query("INSERT INTO proposal_debate_trail (id,proposal_id,kind,source_task_id,created_at) VALUES ('ambiguous-debate','p-overlap','objection','task-ambiguous','2025-01-01T10:02:00Z')").execute(&pool).await.unwrap();
        lifecycle(&pool,"overlap-stop","p-overlap","refinement_stop","2025-01-01T10:03:00Z",Some(serde_json::json!({"reason_tag":"round_cap"}))).await;
        lifecycle(&pool,"solo-start","p-solo","refinement_start","2025-01-01T10:10:00Z",None).await;
        lifecycle(&pool,"solo-row","p-solo","status_change","2025-01-01T10:11:00Z",None).await;
        sqlx::query("INSERT INTO proposal_debate_trail (id,proposal_id,kind,source_task_id,created_at) VALUES ('solo-debate','p-solo','objection','task-solo','2025-01-01T10:11:00Z')").execute(&pool).await.unwrap();
        lifecycle(&pool,"solo-stop","p-solo","refinement_stop","2025-01-01T10:12:00Z",Some(serde_json::json!({"reason_tag":"repeated_objection","objections":["same"]}))).await;
        let canonical = ["adversary_dry", "round_cap", "spawn_cap", "repeated_objection", "agent_failure", "human_accepted", "human_rejected", "interrupted", "reaped_phantom", "operator_stop", "unknown_legacy"];
        for (n, tag) in canonical.iter().enumerate() {
            let p = if n % 2 == 0 { "p-1" } else { "p-2" };
            lifecycle(&pool, &format!("s{n}"), p, "refinement_start", &format!("2025-01-01T00:{n:02}:00Z"), None).await;
            let metadata = if *tag == "repeated_objection" { serde_json::json!({"reason_tag":tag,"objections":["same"]}) }
                else if *tag == "agent_failure" { serde_json::json!({"reason_tag":tag,"agent":"judge","error":{"code":"x"}}) }
                else { serde_json::json!({"reason_tag":tag}) };
            lifecycle(&pool, &format!("e{n}"), p, "refinement_stop", &format!("2025-01-01T00:{:02}:30Z", n), Some(metadata)).await;
        }
        lifecycle(&pool,"alias-start","p-1","refinement_start","2025-01-02T00:00:00Z",None).await;
        lifecycle(&pool,"alias-stop","p-1","refinement_stop","2025-01-02T00:01:00Z",Some(serde_json::json!({"reason":"judge_converged"}))).await;
        lifecycle(&pool,"dry-start","p-2","refinement_start","2025-01-02T00:02:00Z",None).await;
        lifecycle(&pool,"dry-stop","p-2","refinement_stop","2025-01-02T00:03:00Z",Some(serde_json::json!({"stop_reason":"dry_rounds"}))).await;
        lifecycle(&pool,"null-start","p-1","refinement_start","2025-01-03T00:00:00Z",None).await;
        lifecycle(&pool,"null-stop","p-1","refinement_stop","2025-01-03T00:01:00Z",None).await;
        lifecycle(&pool,"unknown-start","p-2","refinement_start","2025-01-03T00:02:00Z",None).await;
        lifecycle(&pool,"unknown-stop","p-2","refinement_stop","2025-01-03T00:03:00Z",Some(serde_json::json!({"reason_tag":"weird"}))).await;
        lifecycle(&pool,"legacy-canonical-start","p-1","refinement_start","2025-01-03T00:04:00Z",None).await;
        lifecycle(&pool,"legacy-canonical-stop","p-1","refinement_stop","2025-01-03T00:05:00Z",Some(serde_json::json!({"reason_tag":"unknown_legacy"}))).await;
        lifecycle(&pool,"ordinary","p-1","status_change","2025-01-04T00:00:00Z",Some(serde_json::json!({"keep":"me"}))).await;
        drop(pool); migration(&mut c).await; drop(c);
        let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
        let tags: Vec<Option<String>> = sqlx::query_scalar("SELECT refinement_stop_tag FROM proposal_revisions WHERE event_kind='refinement_stop' ORDER BY id").fetch_all(&pool).await.unwrap();
        assert!(canonical.iter().all(|x| tags.contains(&Some(x.to_string()))));
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT refinement_stop_tag FROM proposal_revisions WHERE id='alias-stop'").fetch_one(&pool).await.unwrap(), "adversary_dry");
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT refinement_stop_tag FROM proposal_revisions WHERE id='dry-stop'").fetch_one(&pool).await.unwrap(), "adversary_dry");
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT refinement_stop_tag FROM proposal_revisions WHERE id='null-stop'").fetch_one(&pool).await.unwrap(), "unknown_legacy");
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT refinement_stop_tag FROM proposal_revisions WHERE id='unknown-stop'").fetch_one(&pool).await.unwrap(), "unknown_legacy");
        let non_stop: Option<String> = sqlx::query_scalar("SELECT refinement_stop_tag FROM proposal_revisions WHERE id='ordinary'").fetch_one(&pool).await.unwrap(); assert_eq!(non_stop, None);
        let repeated: serde_json::Value = sqlx::query_scalar("SELECT refinement_stop_context FROM proposal_revisions WHERE id='e3'").fetch_one(&pool).await.unwrap(); assert_eq!(repeated["legacy_metadata"]["objections"][0], "same");
        let agent: serde_json::Value = sqlx::query_scalar("SELECT refinement_stop_context FROM proposal_revisions WHERE id='e4'").fetch_one(&pool).await.unwrap();
        assert_eq!(agent["legacy_metadata"]["agent"], "judge");
        assert_eq!(agent["legacy_metadata"]["error"]["code"], "x");
        let unknown_context: serde_json::Value = sqlx::query_scalar("SELECT refinement_stop_context FROM proposal_revisions WHERE id='unknown-stop'").fetch_one(&pool).await.unwrap();
        assert_eq!(unknown_context["legacy_metadata"]["reason_tag"], "weird");
        let null_context: serde_json::Value = sqlx::query_scalar("SELECT refinement_stop_context FROM proposal_revisions WHERE id='null-stop'").fetch_one(&pool).await.unwrap();
        assert_eq!(null_context["legacy_metadata"], serde_json::json!({}));
        let solo = "102fd27f-cb99-4eb3-3e33-04fe1697ae91";
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT id FROM refinement_runs WHERE source_start_revision_id='solo-start'").fetch_one(&pool).await.unwrap(), solo);
        assert_eq!(sqlx::query_scalar::<_,String>("SELECT id FROM refinement_runs WHERE source_start_revision_id='a-start'").fetch_one(&pool).await.unwrap(), "bf01c518-caf4-9a61-6be5-2a24843d4e01");
        assert_eq!(sqlx::query_scalar::<_,i32>("SELECT generation FROM refinement_runs WHERE source_start_revision_id='a-start'").fetch_one(&pool).await.unwrap(), 1);
        assert_eq!(sqlx::query_scalar::<_,i32>("SELECT generation FROM refinement_runs WHERE source_start_revision_id='b-start'").fetch_one(&pool).await.unwrap(), 2);
        for id in ["solo-start", "solo-row", "solo-stop"] { assert_eq!(sqlx::query_scalar::<_,Option<String>>("SELECT refinement_run_id FROM proposal_revisions WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap().as_deref(), Some(solo)); }
        assert_eq!(sqlx::query_scalar::<_,Option<String>>("SELECT refinement_run_id FROM proposal_revisions WHERE id='ambiguous'").fetch_one(&pool).await.unwrap(), None);
        assert_eq!(sqlx::query_scalar::<_,Option<String>>("SELECT refinement_run_id FROM proposal_revisions WHERE id='overlap-stop'").fetch_one(&pool).await.unwrap(), None);
        assert_eq!(sqlx::query_scalar::<_,Option<String>>("SELECT refinement_run_id FROM proposal_debate_trail WHERE id='ambiguous-debate'").fetch_one(&pool).await.unwrap(), None);
        assert_eq!(sqlx::query_scalar::<_,Option<String>>("SELECT refinement_run_id FROM tasks WHERE id='task-ambiguous'").fetch_one(&pool).await.unwrap(), None);
        assert_eq!(sqlx::query_scalar::<_,Option<String>>("SELECT refinement_run_id FROM proposal_debate_trail WHERE id='solo-debate'").fetch_one(&pool).await.unwrap().as_deref(), Some(solo));
        assert_eq!(sqlx::query_scalar::<_,Option<String>>("SELECT refinement_run_id FROM tasks WHERE id='task-solo'").fetch_one(&pool).await.unwrap().as_deref(), Some(solo));
        let ordinary_metadata: serde_json::Value = sqlx::query_scalar("SELECT event_metadata FROM proposal_revisions WHERE id='ordinary'").fetch_one(&pool).await.unwrap();
        assert_eq!(ordinary_metadata, serde_json::json!({"keep":"me"}));
        pool.close().await;
    }).await;
}

#[tokio::test]
async fn migration_138_constraints_allow_successor_after_terminal() {
    temporary("constraints", |url| async move {
        let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
        sqlx::migrate!("./migrations_postgres").run(&pool).await.unwrap(); proposal(&pool,"p","p000").await;
        let run = "00000000-0000-0000-0000-000000000001";
        sqlx::query("INSERT INTO refinement_runs (id,proposal_id,generation,idempotency_key,state) VALUES ($1,'p',1,'one','running')").bind(run).execute(&pool).await.unwrap();
        assert!(sqlx::query("INSERT INTO refinement_runs (id,proposal_id,generation,idempotency_key,state) VALUES ('00000000-0000-0000-0000-000000000002','p',2,'two','running')").execute(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO refinement_dispatch_intents (id,run_id,round,phase,role,idempotency_key) VALUES ('00000000-0000-0000-0000-000000000003',$1,1,'debate','judge','i')").bind(run).execute(&pool).await.is_ok());
        assert!(sqlx::query("INSERT INTO refinement_dispatch_intents (id,run_id,round,phase,role,idempotency_key) VALUES ('00000000-0000-0000-0000-000000000004',$1,1,'debate','judge','j')").bind(run).execute(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO refinement_dispatch_intents (id,run_id,round,phase,role,idempotency_key) VALUES ('00000000-0000-0000-0000-000000000010',$1,2,'debate','judge','i')").bind(run).execute(&pool).await.is_err());
        assert!(sqlx::query("UPDATE refinement_runs SET state='terminal', terminal_at='2025-01-01T00:00:00Z', stop_tag='not-canonical' WHERE id=$1").bind(run).execute(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO proposal_revisions (id,proposal_id,seq,title,body,event_kind,refinement_stop_tag) VALUES ('bad-stop','p',1,'','','refinement_stop','not-canonical')").execute(&pool).await.is_err());
        assert!(sqlx::query("UPDATE refinement_runs SET state='terminal', terminal_at='2025-01-01T00:00:00Z', stop_tag='unknown_legacy' WHERE id=$1").bind(run).execute(&pool).await.is_ok());
        assert!(sqlx::query("INSERT INTO refinement_runs (id,proposal_id,generation,idempotency_key,state,terminal_at,stop_tag) VALUES ('00000000-0000-0000-0000-000000000008','p',1,'different','terminal','2025-01-01T00:00:00Z','unknown_legacy')").execute(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO refinement_runs (id,proposal_id,generation,idempotency_key,state,terminal_at,stop_tag) VALUES ('00000000-0000-0000-0000-000000000009','p',2,'one','terminal','2025-01-01T00:00:00Z','unknown_legacy')").execute(&pool).await.is_err());
        assert!(sqlx::query("INSERT INTO refinement_runs (id,proposal_id,generation,idempotency_key,state) VALUES ('00000000-0000-0000-0000-000000000005','p',2,'two','running')").execute(&pool).await.is_ok());
        pool.close().await;
    }).await;
}

#[tokio::test]
async fn migration_138_fresh_application_preserves_ordinary_rows() {
    temporary("fresh", |url| async move {
        let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
        sqlx::migrate!("./migrations_postgres").run(&pool).await.unwrap();
        proposal(&pool, "p", "p000").await;
        lifecycle(&pool, "ordinary", "p", "status_change", "2025-01-01T00:00:00Z", Some(serde_json::json!({"legacy":true}))).await;
        let row: (Option<String>, Option<String>, serde_json::Value) = sqlx::query_as("SELECT refinement_run_id, refinement_stop_tag, event_metadata FROM proposal_revisions WHERE id='ordinary'").fetch_one(&pool).await.unwrap();
        assert_eq!(row.0, None); assert_eq!(row.1, None); assert_eq!(row.2, serde_json::json!({"legacy":true}));
        pool.close().await;
    }).await;
}

async fn deterministic_snapshot(url: &str) -> (String, i32) {
    let mut connection = PgConnection::connect(url).await.unwrap();
    prior(&mut connection).await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    proposal(&pool, "deterministic", "pd001").await;
    lifecycle(
        &pool,
        "deterministic-start",
        "deterministic",
        "refinement_start",
        "2025-01-01T00:00:00Z",
        None,
    )
    .await;
    lifecycle(
        &pool,
        "deterministic-stop",
        "deterministic",
        "refinement_stop",
        "2025-01-01T00:01:00Z",
        Some(serde_json::json!({"reason_tag":"round_cap"})),
    )
    .await;
    drop(pool);
    migration(&mut connection).await;
    drop(connection);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .unwrap();
    let result = sqlx::query_as("SELECT id, generation FROM refinement_runs WHERE source_start_revision_id='deterministic-start'").fetch_one(&pool).await.unwrap();
    pool.close().await;
    result
}

#[tokio::test]
async fn migration_138_reapplication_on_identical_history_is_deterministic() {
    let first = temporary("deterministic_a", |url| async move {
        deterministic_snapshot(&url).await
    })
    .await;
    let second = temporary("deterministic_b", |url| async move {
        deterministic_snapshot(&url).await
    })
    .await;
    assert_eq!(first, second);
}
