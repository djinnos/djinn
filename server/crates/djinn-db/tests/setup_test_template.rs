// Test helper: println used for migration progress output with --nocapture.
#![allow(clippy::print_stdout)]
//! One-shot setup helper: build the `djinn_test_template` database on the
//! configured DJINN_TEST_DATABASE_URL (default = the cluster Postgres) and
//! apply every `migrations_postgres/*.sql` file. Run with:
//!   cargo test -p djinn-db --test setup_test_template -- --ignored --nocapture
//!
//! Idempotent: drops the template first if it exists, then re-creates it.
//! Marks the result as a TEMPLATE so `Database::open_in_memory()` can clone
//! it via `CREATE DATABASE x TEMPLATE djinn_test_template`.

#[tokio::test]
#[ignore]
async fn setup_test_template() {
    use sqlx::Connection;
    use sqlx::postgres::PgConnection;
    use std::path::Path;

    let base = std::env::var("DJINN_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://djinn:VipjO1uAdxAGvNSA6EcJdZMdHAquYeJj@djinn-postgres.djinn.svc.cluster.local:5432/djinn".to_owned());
    // Strip the trailing `/<db>` to get the server prefix.
    let server_prefix = base
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(&base)
        .to_owned();
    let admin_url = format!("{server_prefix}/postgres");

    let mut conn = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = 'djinn_test_template' AND pid <> pg_backend_pid()",
    )
    .execute(&mut conn)
    .await;
    let _ = sqlx::query("DROP DATABASE IF EXISTS djinn_test_template")
        .execute(&mut conn)
        .await;
    sqlx::query("CREATE DATABASE djinn_test_template")
        .execute(&mut conn)
        .await
        .expect("create template");
    drop(conn);

    let template_url = format!("{server_prefix}/djinn_test_template");
    let mut conn = PgConnection::connect(&template_url)
        .await
        .expect("connect template");
    let migrations_dir = std::env::var("DJINN_MIGRATIONS_DIR").unwrap_or_else(|_| {
        // Walk up from the crate's CARGO_MANIFEST_DIR to the workspace root
        // and find the migrations_postgres directory.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let mut p = std::path::PathBuf::from(manifest_dir);
        p.pop();
        p.pop();
        p.push("crates/djinn-db/migrations_postgres");
        p.to_string_lossy().into_owned()
    });
    let migrations = Path::new(&migrations_dir);
    let mut entries: Vec<_> = std::fs::read_dir(migrations)
        .expect("read migrations dir")
        .map(|e| e.unwrap().path())
        .collect();
    // sqlx-cli (and the codebase in general) sort migrations by their
    // numeric prefix; a plain lexicographic sort happens to work for the
    // 2-digit prefixes in this repo, but a 3-digit rename would silently
    // misorder. Sort by `(numeric_prefix, full_name)` instead.
    entries.sort_by(|a, b| {
        let ak = a.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let bk = b.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let a_prefix: u64 = ak
            .split('_')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        let b_prefix: u64 = bk
            .split('_')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        a_prefix.cmp(&b_prefix).then(ak.cmp(bk))
    });
    for path in entries {
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read sql");
        println!("Applying: {}", path.file_name().unwrap().to_string_lossy());
        // Migration files contain multiple statements separated by `;`; the
        // prepared-statement path only takes one at a time. Running through
        // `Executor::execute(&str)` uses the simple query protocol, which
        // accepts a multi-statement string in a single round trip.
        use sqlx::Executor;
        conn.execute(sql.as_str()).await.expect("apply migration");
    }
    drop(conn);

    let mut conn = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect admin");
    sqlx::query(
        "UPDATE pg_database SET datistemplate = TRUE WHERE datname = 'djinn_test_template'",
    )
    .execute(&mut conn)
    .await
    .expect("mark template");
    println!("djinn_test_template ready at {template_url}");
}
