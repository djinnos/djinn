//! Contract coverage for migration 176's dormant feedback-refinement storage.
use sqlx::Connection;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn migration_176_preserves_legacy_feedback_and_enforces_generation_contract() {
    let base = djinn_db::test_database_base_url();
    let prefix = base
        .rsplit_once('/')
        .map(|v| v.0)
        .unwrap_or(&base)
        .trim_end_matches('/');
    let name = format!(
        "djinn_feedback_refinement_{}",
        uuid::Uuid::now_v7().simple()
    );
    let admin = format!("{prefix}/postgres");
    let mut admin_connection = sqlx::postgres::PgConnection::connect(&admin).await.unwrap();
    sqlx::query(&format!(r#"CREATE DATABASE "{name}""#))
        .execute(&mut admin_connection)
        .await
        .unwrap();
    let url = format!("{prefix}/{name}");
    djinn_db::test_support::apply_all_migrations_to_fresh_database(&url).await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("INSERT INTO proposals (id,short_id,title) VALUES ('p','p173','legacy')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO proposal_feedback (id,proposal_id,body) VALUES ('root','p','legacy feedback'),('root2','p','second root'),('reply','p','reply')").execute(&pool).await.unwrap();
    let severity: String =
        sqlx::query_scalar("SELECT severity FROM proposal_feedback WHERE id='root'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(severity, "blocking");
    assert!(
        sqlx::query("UPDATE proposal_feedback SET severity='bad' WHERE id='root'")
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO proposal_debate_trail (id,proposal_id,kind) VALUES ('debate','p','human_feedback'),('debate2','p','human_feedback')").execute(&pool).await.unwrap();
    let insert = "INSERT INTO proposal_feedback_refinement_injections (id,proposal_id,root_feedback_id,generation,cutoff_at,cutoff_feedback_id,round,debate_entry_id) VALUES ($1,'p','root',1,'2025-01-01T00:00:00Z','reply',1,'debate')";
    sqlx::query(insert).bind("i1").execute(&pool).await.unwrap();
    let duplicate_generation = "INSERT INTO proposal_feedback_refinement_injections (id,proposal_id,root_feedback_id,generation,cutoff_at,cutoff_feedback_id,round,debate_entry_id) VALUES ('i2','p','root',1,'2025-01-01T00:00:00Z','reply',1,'debate2')";
    assert!(
        sqlx::query(duplicate_generation)
            .execute(&pool)
            .await
            .is_err()
    );
    let duplicate_debate = "INSERT INTO proposal_feedback_refinement_injections (id,proposal_id,root_feedback_id,generation,cutoff_at,cutoff_feedback_id,round,debate_entry_id) VALUES ('i3','p','root2',1,'2025-01-01T00:00:00Z','reply',1,'debate')";
    assert!(sqlx::query(duplicate_debate).execute(&pool).await.is_err());
    sqlx::query("INSERT INTO proposal_feedback_refinement_sources (injection_id,source_feedback_id,source_ordinal,source_author_kind,source_body,source_severity,source_created_at,captured_at) VALUES ('i1','root',1,'user','root verbatim','blocking','2025-01-01T00:00:00Z','2025-01-01T00:00:00Z'),('i1','reply',2,'user','reply verbatim','blocking','2025-01-01T00:00:01Z','2025-01-01T00:00:02Z')").execute(&pool).await.unwrap();
    let ordered_source_ids: Vec<String> = sqlx::query_scalar(
        "SELECT source_feedback_id FROM proposal_feedback_refinement_sources WHERE injection_id='i1' ORDER BY source_ordinal",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(ordered_source_ids, ["root", "reply"]);
    assert!(sqlx::query("INSERT INTO proposal_feedback_refinement_sources (injection_id,source_feedback_id,source_ordinal,source_author_kind,source_body,source_severity,source_created_at,captured_at) VALUES ('i1','root2',1,'user','duplicate ordinal','blocking','2025-01-01T00:00:03Z','2025-01-01T00:00:04Z')").execute(&pool).await.is_err());
    pool.close().await;
    let mut c = sqlx::postgres::PgConnection::connect(&admin).await.unwrap();
    let _ = sqlx::Executor::execute(&mut c, format!(r#"DROP DATABASE "{name}""#).as_str()).await;
}
