//! Focused live-Postgres regressions for migration 125 publication-lock evidence.

use std::path::{Path, PathBuf};

use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor, Row};

const MIGRATION_VERSION: u64 = 125;

fn base_database_url() -> String {
    djinn_db::test_database_base_url()
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read migrations directory")
        .map(|entry| {
            let path = entry.expect("migration entry").path();
            let version = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.split_once('_'))
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
        .collect();
    entries.sort_by_key(|(version, _)| *version);
    entries
}

async fn apply_through_125(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version == 0 || version > MIGRATION_VERSION {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply migration {}: {error}", path.display()));
    }
}

async fn assert_update_rejected(conn: &mut PgConnection) {
    let error = conn
        .execute("UPDATE repo_graph_cache SET graph_blob = decode('02', 'hex') WHERE project_id = 'lock-project'")
        .await
        .expect_err("unlocked compatibility UPDATE must fail");
    assert!(
        error
            .to_string()
            .contains("requires its project publication lock"),
        "unexpected UPDATE error: {error}"
    );
}

async fn assert_bad_manifest_rejected(conn: &mut PgConnection, manifest: &str) {
    let generation = uuid::Uuid::now_v7();
    let artifact = uuid::Uuid::now_v7();
    conn.execute("BEGIN")
        .await
        .expect("begin marked publication");
    sqlx::query("SELECT repo_graph_reserve_generation('lock-project', $1::text::uuid)")
        .bind(generation.to_string())
        .execute(&mut *conn)
        .await
        .expect("reserve generation");
    sqlx::query(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at, generation_id) \
         VALUES ('lock-project', $1, decode('03', 'hex'), '', $2::text::uuid)",
    )
    .bind(generation.to_string())
    .bind(generation.to_string())
    .execute(&mut *conn)
    .await
    .expect("insert marked compatibility row");
    sqlx::query(
        "INSERT INTO repo_graph_galaxy_artifact \
         (artifact_id, generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
         VALUES ($1::text::uuid, $2::text::uuid, 'content', 'transport', 1, 1, $3::jsonb)",
    )
    .bind(artifact.to_string())
    .bind(generation.to_string())
    .bind(manifest)
    .execute(&mut *conn)
    .await
    .expect("insert invalid manifest for deferred validation");
    sqlx::query(
        "INSERT INTO repo_graph_galaxy_chunk \
         (generation_id, artifact_id, chunk_index, byte_count, sha256, bytes) \
         VALUES ($1::text::uuid, $2::text::uuid, 0, 1, 'actual-hash', decode('ff', 'hex'))",
    )
    .bind(generation.to_string())
    .bind(artifact.to_string())
    .execute(&mut *conn)
    .await
    .expect("insert chunk");
    let error = conn
        .execute("COMMIT")
        .await
        .expect_err("null/non-text manifest hash must fail deferred validation");
    assert!(
        error
            .to_string()
            .contains("artifact chunks are incomplete or invalid"),
        "unexpected manifest error: {error}"
    );
}

#[tokio::test]
async fn publication_tokens_are_guarded_by_the_project_xact_lock() {
    let prefix = server_prefix(&base_database_url());
    let db_name = format!("djinn_graph_lock_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect postgres admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create migration test database");
    drop(admin);

    let db_url = format!("{prefix}/{db_name}");
    let mut owner = PgConnection::connect(&db_url).await.expect("connect owner");
    apply_through_125(&mut owner).await;
    owner
        .execute(
            "INSERT INTO projects(id, name, github_owner, github_repo) \
             VALUES ('lock-project', 'lock project', 'lock-owner', 'lock-repo')",
        )
        .await
        .expect("insert project");
    owner
        .execute(
            "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
             VALUES ('lock-project', 'initial', decode('01', 'hex'), '')",
        )
        .await
        .expect("seed compatibility row");

    // A forgeable custom GUC is not evidence.
    owner.execute("BEGIN").await.expect("begin GUC attack");
    owner
        .execute("SET LOCAL djinn.repo_graph_publish_lock_project = 'lock-project'")
        .await
        .expect("forge custom GUC");
    assert_update_rejected(&mut owner).await;
    owner
        .execute("ROLLBACK")
        .await
        .expect("rollback GUC attack");

    // A session advisory lock is not transaction-token evidence.
    owner
        .execute("SELECT pg_advisory_lock(hashtextextended('lock-project', 0))")
        .await
        .expect("take session advisory lock");
    owner
        .execute("BEGIN")
        .await
        .expect("begin session-lock attack");
    assert_update_rejected(&mut owner).await;
    owner
        .execute("ROLLBACK")
        .await
        .expect("rollback session-lock attack");
    owner
        .execute("SELECT pg_advisory_unlock(hashtextextended('lock-project', 0))")
        .await
        .expect("release session advisory lock");

    // Direct INSERT cannot become visible while another publisher owns the
    // project xact lock: its BEFORE trigger must attempt that same lock first.
    owner
        .execute("BEGIN")
        .await
        .expect("begin legitimate publisher");
    owner
        .execute("SELECT repo_graph_acquire_publish_lock('lock-project')")
        .await
        .expect("acquire legitimate publication lock");
    let mut attacker = PgConnection::connect(&db_url)
        .await
        .expect("connect attacker");
    attacker
        .execute("SET statement_timeout = '200ms'")
        .await
        .expect("set attack timeout");
    let blocked = attacker
        .execute(
            "INSERT INTO repo_graph_publish_lock_token(project_id, transaction_id, backend_pid) \
             VALUES ('lock-project', txid_current(), pg_backend_pid())",
        )
        .await
        .expect_err("direct token INSERT must wait for the project xact lock");
    assert!(
        blocked.to_string().contains("statement timeout"),
        "{blocked}"
    );
    owner
        .execute("ROLLBACK")
        .await
        .expect("release legitimate publication lock");

    // Once unblocked, direct INSERT is safe because the trigger overwrites
    // forged identity and necessarily acquires the transaction-scoped lock.
    attacker.execute("SET statement_timeout = 0").await.unwrap();
    attacker.execute("BEGIN").await.unwrap();
    attacker
        .execute(
            "INSERT INTO repo_graph_publish_lock_token \
             (project_id, transaction_id, backend_pid) VALUES ('lock-project', 1, 1)",
        )
        .await
        .expect("guarded direct insertion");
    let identity = sqlx::query(
        "SELECT transaction_id = txid_current() AS xid_ok, backend_pid = pg_backend_pid() AS pid_ok \
         FROM repo_graph_publish_lock_token WHERE project_id = 'lock-project'",
    )
    .fetch_one(&mut attacker)
    .await
    .expect("read stamped identity");
    assert!(identity.get::<bool, _>("xid_ok"));
    assert!(identity.get::<bool, _>("pid_ok"));
    attacker
        .execute("UPDATE repo_graph_cache SET graph_blob = decode('04', 'hex') WHERE project_id = 'lock-project'")
        .await
        .expect("guarded token permits compatibility update");
    let mutation = attacker
        .execute(
            "UPDATE repo_graph_publish_lock_token SET reserved_generation = gen_random_uuid() \
             WHERE project_id = 'lock-project'",
        )
        .await
        .expect_err("reserved generation must be immutable");
    assert!(
        mutation.to_string().contains("tokens are immutable"),
        "{mutation}"
    );
    attacker.execute("ROLLBACK").await.unwrap();

    assert_bad_manifest_rejected(&mut owner, "[null]").await;
    assert_bad_manifest_rejected(&mut owner, "[123]").await;
    drop(attacker);
    drop(owner);

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect admin");
    admin
        .execute(format!(r#"DROP DATABASE "{db_name}""#).as_str())
        .await
        .expect("drop migration test database");
}
