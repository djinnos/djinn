use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use tokio::sync::OnceCell;

use crate::error::{DbError, DbResult};
use crate::migrations;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseBackendKind {
    Mysql,
}

impl DatabaseBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mysql => "mysql",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MysqlBackendFlavor {
    Mysql,
    Dolt,
}

impl MysqlBackendFlavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mysql => "mysql",
            Self::Dolt => "dolt",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum DatabaseConnectConfig {
    Mysql(MysqlDatabaseConfig),
}

impl DatabaseConnectConfig {
    pub fn backend_kind(&self) -> DatabaseBackendKind {
        match self {
            Self::Mysql(_) => DatabaseBackendKind::Mysql,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MysqlDatabaseConfig {
    pub url: String,
    #[serde(default = "default_mysql_flavor")]
    pub flavor: MysqlBackendFlavor,
}

impl MysqlDatabaseConfig {
    pub fn display_backend(&self) -> &'static str {
        self.flavor.as_str()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseBootstrapInfo {
    pub backend_kind: DatabaseBackendKind,
    pub backend_label: String,
    pub target: String,
    pub readonly: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteSearchBackend {
    SqliteFts5,
    MysqlFulltext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteVectorBackend {
    SqliteVec,
    External,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseBackendCapabilities {
    pub backend_kind: DatabaseBackendKind,
    pub backend_label: String,
    pub lexical_search: NoteSearchBackend,
    pub note_vector_backend: NoteVectorBackend,
    pub supports_sqlite_pragmas: bool,
    pub supports_sqlite_vec: bool,
    pub supports_branching_metadata: bool,
    pub supports_readonly_connection_mode: bool,
}

fn default_mysql_flavor() -> MysqlBackendFlavor {
    MysqlBackendFlavor::Mysql
}

#[derive(Clone, Debug)]
pub struct Database {
    pool: MySqlPool,
    readonly: bool,
    bootstrap: DatabaseBootstrapInfo,
    capabilities: DatabaseBackendCapabilities,
    initialized: Arc<OnceCell<()>>,
    test_branch: Option<TestBranchInit>,
}

#[derive(Clone, Debug)]
enum TestBranchInit {
    /// Legacy Dolt branching: a single shared `djinn_test` database with a
    /// branch per test. Slated for deletion in Phase 2.
    Dolt {
        server_prefix: String,
        shared_db: String,
        branch: String,
    },
    /// MySQL template-clone isolation: per-test `djinn_test_<uuid>`
    /// schema cloned from `djinn_test_template`.
    Mysql {
        server_prefix: String,
        test_db: String,
    },
}

/// Process-wide guard: the shared `djinn_test` database is created and
/// migrated exactly once per test process, regardless of how many
/// `open_in_memory()` callers fan out. Branch creation is per-instance.
static SHARED_TEST_DB_INIT: OnceCell<()> = OnceCell::const_new();

impl Database {
    /// Open a database using an explicit backend selection seam.
    pub fn open_with_config(config: DatabaseConnectConfig) -> DbResult<Self> {
        match config {
            // Dolt COMMITs can take 30–90s under write contention (merge-based
            // commit path is O(working-set)), so a connection is held ~10×
            // longer per write than a plain MySQL one. A pool of 8 starves
            // almost immediately when mirror-fetcher + KB watcher + MCP tools
            // all have a write in flight. 32 gives reads + concurrent writers
            // headroom without blowing past Dolt's own connection cap.
            DatabaseConnectConfig::Mysql(mysql) => Self::open_mysql(&mysql, 32),
        }
    }

    /// Open an isolated test database.
    ///
    /// Phase 1 of the Dolt → MySQL cutover supports two backends behind one
    /// API:
    ///
    /// * **MySQL** (when `DJINN_TEST_MYSQL_URL` is set, default in Phase 2):
    ///   create a fresh `djinn_test_<uuid>` schema on `:3308` and clone the
    ///   table definitions from the `djinn_test_template` schema, which was
    ///   pre-built by `make test-db-mysql-template`. Cloning is sub-second
    ///   because the mysql-test container is tmpfs-backed.
    /// * **Dolt** (legacy fallback, kept until Phase 2 deletes it): create a
    ///   fresh branch on the shared `djinn_test` database. Each new pool
    ///   connection runs `USE djinn_test/<branch>` in `after_connect`.
    ///
    /// Each test gets a UUIDv7-named container (database or branch) — no
    /// collisions under `cargo nextest` parallelism.
    pub fn open_in_memory() -> DbResult<Self> {
        if let Ok(base) = std::env::var("DJINN_TEST_MYSQL_URL") {
            return Self::open_in_memory_mysql(&base);
        }
        Self::open_in_memory_dolt()
    }

    /// Legacy Dolt-branch test isolation. See `open_in_memory` for context.
    /// Slated for deletion in Phase 2 of the MySQL cutover.
    fn open_in_memory_dolt() -> DbResult<Self> {
        let server_prefix = "mysql://root@127.0.0.1:3307".to_owned();
        let shared_db = "djinn_test".to_owned();
        let branch = format!("t_{}", uuid::Uuid::now_v7().simple());
        // Pool connects to the shared DB; after_connect pins each conn to
        // the branch via `USE djinn_test/<branch>`.
        let url = format!("{server_prefix}/{shared_db}");

        Self::open_mysql_inner(
            &MysqlDatabaseConfig {
                url,
                flavor: MysqlBackendFlavor::Dolt,
            },
            4,
            Some(TestBranchInit::Dolt {
                server_prefix,
                shared_db,
                branch,
            }),
        )
    }

    /// MySQL template-clone test isolation.
    ///
    /// Takes the base URL (e.g. `mysql://root@127.0.0.1:3308`), strips any
    /// trailing `/<db>` segment, then constructs an admin connection
    /// against `<base>/mysql`, creates a fresh `djinn_test_<uuid>` schema,
    /// and clones every table from `djinn_test_template` into it
    /// (DDL + rows; the only seeded rows are in `_sqlx_migrations`).
    ///
    /// FK constraints are recreated by replaying each table's
    /// `SHOW CREATE TABLE` output with `FOREIGN_KEY_CHECKS=0` for the
    /// duration of the clone — `CREATE TABLE ... LIKE` would silently drop
    /// the FK constraints, breaking ON DELETE CASCADE assumptions in many
    /// repositories.
    fn open_in_memory_mysql(base: &str) -> DbResult<Self> {
        let server_prefix = strip_server_prefix(base);
        let test_db = format!("djinn_test_{}", uuid::Uuid::now_v7().simple());
        let url = format!("{server_prefix}/{test_db}");

        Self::open_mysql_inner(
            &MysqlDatabaseConfig {
                url,
                flavor: MysqlBackendFlavor::Mysql,
            },
            4,
            Some(TestBranchInit::Mysql {
                server_prefix,
                test_db,
            }),
        )
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    pub fn mysql_pool(&self) -> Option<&MySqlPool> {
        Some(&self.pool)
    }

    pub fn bootstrap_info(&self) -> &DatabaseBootstrapInfo {
        &self.bootstrap
    }

    pub fn backend_capabilities(&self) -> &DatabaseBackendCapabilities {
        &self.capabilities
    }

    pub fn backend_kind(&self) -> DatabaseBackendKind {
        self.bootstrap.backend_kind
    }

    fn open_mysql(config: &MysqlDatabaseConfig, max_connections: u32) -> DbResult<Self> {
        Self::open_mysql_inner(config, max_connections, None)
    }

    fn open_mysql_inner(
        config: &MysqlDatabaseConfig,
        max_connections: u32,
        test_branch: Option<TestBranchInit>,
    ) -> DbResult<Self> {
        let mut opts = MySqlConnectOptions::from_str(&config.url)?;
        // For the MySQL test path, disable sqlx's prepared-statement cache.
        // Parallel tests rapidly CREATE / DROP databases with the same table
        // names (`djinn_test_<uuid>` instances cloned from the same
        // template); MySQL's server-side statement cache then surfaces
        // error 1615 ("Prepared statement needs to be re-prepared") when a
        // cached statement is reused against a freshly-created twin schema.
        // Disabling client-side caching forces a fresh PREPARE on each
        // query, which is a small per-test overhead and eliminates the race.
        // Production / Dolt paths keep caching on.
        if matches!(test_branch, Some(TestBranchInit::Mysql { .. })) {
            opts = opts.statement_cache_capacity(0);
        }
        // Precompute the `USE db/branch` statement so the after_connect
        // closure can be shared (no per-connection allocation). Only the
        // Dolt branch variant needs it — the MySQL template-clone path
        // points the connection URL directly at `djinn_test_<uuid>`.
        let use_branch_stmt = test_branch.as_ref().and_then(|init| match init {
            TestBranchInit::Dolt {
                shared_db, branch, ..
            } => Some(format!("USE {shared_db}/{branch}")),
            TestBranchInit::Mysql { .. } => None,
        });
        let pool = MySqlPoolOptions::new()
            .max_connections(max_connections)
            // Dolt COMMITs can run 30–90s under write contention; the sqlx
            // default acquire_timeout of 30s causes every UI/MCP request
            // queued behind a slow write to hard-fail with "pool timed out"
            // even though the DB is healthy. Raise the ceiling so queued
            // requests wait it out instead of cascading into retry storms.
            .acquire_timeout(std::time::Duration::from_secs(120))
            .after_connect(move |conn, _meta| {
                let use_branch_stmt = use_branch_stmt.clone();
                Box::pin(async move {
                    sqlx::query("SET SESSION sql_mode = CONCAT(@@sql_mode, ',STRICT_ALL_TABLES')")
                        .execute(&mut *conn)
                        .await?;
                    // Bound how long any single statement / lock wait can
                    // hang. Dolt's enforcement of these is best-effort
                    // (varies by version) — the dolt_watchdog spawned at
                    // server startup is the authoritative safety net. Both
                    // SETs are wrapped in best-effort blocks so a Dolt
                    // build that rejects them with a parse error doesn't
                    // break new connections; older Dolt accepts MySQL
                    // syntax but ignores the value.
                    let _ = sqlx::query("SET SESSION MAX_EXECUTION_TIME = 60000")
                        .execute(&mut *conn)
                        .await;
                    let _ = sqlx::query("SET SESSION lock_wait_timeout = 60")
                        .execute(&mut *conn)
                        .await;
                    if let Some(stmt) = use_branch_stmt {
                        sqlx::query(&stmt).execute(&mut *conn).await?;
                    }
                    Ok(())
                })
            })
            .connect_lazy_with(opts);

        let backend_label = config.display_backend().to_owned();
        Ok(Self {
            pool,
            readonly: false,
            bootstrap: DatabaseBootstrapInfo {
                backend_kind: DatabaseBackendKind::Mysql,
                backend_label: backend_label.clone(),
                target: config.url.clone(),
                readonly: false,
            },
            capabilities: DatabaseBackendCapabilities {
                backend_kind: DatabaseBackendKind::Mysql,
                backend_label,
                lexical_search: NoteSearchBackend::MysqlFulltext,
                note_vector_backend: NoteVectorBackend::External,
                supports_sqlite_pragmas: false,
                supports_sqlite_vec: false,
                supports_branching_metadata: matches!(config.flavor, MysqlBackendFlavor::Dolt),
                supports_readonly_connection_mode: false,
            },
            initialized: Arc::new(OnceCell::new()),
            test_branch,
        })
    }

    /// Legacy stub retained so sqlite-vec-era embedding tests still compile.
    ///
    /// MySQL/Dolt has no sqlite-vec equivalent — vector search is handled by
    /// Qdrant. This always returns "unavailable" so tests that gate on the
    /// extension status take their fallback path.
    /// Return `true` if a table with the given name exists in the current
    /// database schema.
    ///
    /// Exists as a deliberate test fixture: contract tests verify that the
    /// in-memory test database has its migrations applied. Production code
    /// should not need to probe the schema at runtime.
    pub async fn table_exists(&self, table_name: &str) -> DbResult<bool> {
        self.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = ?",
        )
        .bind(table_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    pub async fn sqlite_vec_status(&self) -> DbResult<SqliteVecStatus> {
        Ok(SqliteVecStatus {
            available: false,
            version: None,
            detail: Some("sqlite-vec retired with the MySQL/Dolt migration".to_owned()),
        })
    }

    /// Read-only sanity check: confirm every migration baked into this
    /// binary has already been applied successfully on the live DB.
    ///
    /// **Does not** call `sqlx::migrate!(...).run(...)` — that path takes
    /// the migrator's advisory lock (`GET_LOCK(_sqlx_migrator_lock, -1)`).
    /// If the previous pod's connection died holding a partition lock on
    /// some table being ALTERed, that advisory lock can survive the
    /// connection drop and wedge the new pod indefinitely.
    ///
    /// Cut-over: migrations now live in the Helm `pre-upgrade` Job
    /// (`djinn-server --migrate-only`). App pods just verify the schema
    /// is current and refuse to start otherwise — a much faster, lock-free
    /// boot path that no longer fights Dolt over the migrator lock.
    ///
    /// Returns `Err` when:
    ///   * The DB doesn't have a `_sqlx_migrations` table at all (fresh
    ///     DB — the Job must run first).
    ///   * Any migration version known to the binary is missing from the
    ///     DB or is recorded as `success = 0` (Job didn't finish).
    pub async fn verify_schema_is_current(&self) -> DbResult<()> {
        if self.readonly {
            return Ok(());
        }

        let migrator = sqlx::migrate!("./migrations_mysql");
        let expected_versions: Vec<i64> = migrator.migrations.iter().map(|m| m.version).collect();
        if expected_versions.is_empty() {
            return Ok(());
        }

        // Detect the "no migrations table at all" case up front so we can
        // give the operator a clear error message instead of a generic
        // "_sqlx_migrations not found" SQL exception.
        let migrations_table_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM information_schema.tables \
             WHERE table_schema = DATABASE() \
               AND table_name   = '_sqlx_migrations'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from)?;
        if migrations_table_exists == 0 {
            return Err(DbError::InvalidData(
                "_sqlx_migrations table missing — the migration Job has not run yet. \
                 Run `djinn-server --migrate-only` against this database before booting \
                 the app pod (the Helm `pre-upgrade` hook normally handles this)."
                    .to_owned(),
            ));
        }

        let applied: Vec<(i64, bool)> =
            sqlx::query_as("SELECT version, success FROM _sqlx_migrations")
                .fetch_all(&self.pool)
                .await
                .map_err(DbError::from)?;
        let mut applied_ok = std::collections::HashSet::new();
        for (version, success) in &applied {
            if *success {
                applied_ok.insert(*version);
            }
        }

        let mut missing: Vec<i64> = expected_versions
            .iter()
            .copied()
            .filter(|v| !applied_ok.contains(v))
            .collect();
        missing.sort_unstable();
        if !missing.is_empty() {
            return Err(DbError::InvalidData(format!(
                "schema is behind the binary — missing migrations: {missing:?}. \
                 The Helm `pre-upgrade` Job should have applied these via \
                 `djinn-server --migrate-only` before this pod started.",
            )));
        }

        Ok(())
    }

    pub async fn ensure_initialized(&self) -> DbResult<()> {
        if self.readonly {
            return Ok(());
        }

        let pool = self.pool.clone();
        let url = self.bootstrap.target.clone();
        let test_branch = self.test_branch.clone();
        self.initialized
            .get_or_try_init(|| async move {
                match test_branch {
                    Some(TestBranchInit::Dolt {
                        server_prefix,
                        shared_db,
                        branch,
                    }) => {
                        // Shared djinn_test DB + migrations on `main` —
                        // once per process, regardless of test fan-out.
                        SHARED_TEST_DB_INIT
                            .get_or_try_init(|| {
                                init_shared_test_db(
                                    server_prefix.clone(),
                                    shared_db.clone(),
                                )
                            })
                            .await?;
                        // Per-instance: create the branch this test will
                        // write against. UUIDv7 names don't collide.
                        create_test_branch(&server_prefix, &shared_db, &branch).await?;
                    }
                    Some(TestBranchInit::Mysql {
                        server_prefix,
                        test_db,
                    }) => {
                        // Clone the pre-built `djinn_test_template` schema
                        // into the per-test database. The template Job
                        // (`make test-db-mysql-template`) must have run
                        // already — if it hasn't, the clone fails with a
                        // clear "Unknown database" error and the operator
                        // knows what to do.
                        clone_mysql_test_template(&server_prefix, &test_db).await?;
                    }
                    None => {
                        migrations::ensure_mysql_database_exists(&url).await?;
                        sqlx::migrate!("./migrations_mysql")
                            .run(&pool)
                            .await
                            .map_err(|e: sqlx::migrate::MigrateError| {
                                DbError::InvalidData(e.to_string())
                            })?;
                    }
                }
                Ok::<(), DbError>(())
            })
            .await?;
        Ok(())
    }
}

fn strip_server_prefix(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    match trimmed.strip_prefix("mysql://") {
        Some(after_scheme) => match after_scheme.find('/') {
            Some(slash) => format!("mysql://{}", &after_scheme[..slash]),
            None => format!("mysql://{after_scheme}"),
        },
        None => trimmed.to_owned(),
    }
}

async fn init_shared_test_db(server_prefix: String, shared_db: String) -> DbResult<()> {
    let url = format!("{server_prefix}/{shared_db}");
    migrations::ensure_mysql_database_exists(&url).await?;
    let pool = MySqlPool::connect(&url)
        .await
        .map_err(DbError::from)?;
    let result = async {
        sqlx::migrate!("./migrations_mysql")
            .run(&pool)
            .await
            .map_err(|e: sqlx::migrate::MigrateError| DbError::InvalidData(e.to_string()))?;
        // Dolt branches fork from the latest *commit*, not the working
        // root. Without this commit, `CALL DOLT_BRANCH('t_x', 'main')`
        // would produce a branch with an empty schema because the
        // migrations live in main's unstaged working root.
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_status")
            .fetch_one(&pool)
            .await
            .map_err(DbError::from)?;
        if pending > 0 {
            sqlx::query("CALL DOLT_ADD('-A')")
                .execute(&pool)
                .await
                .map_err(DbError::from)?;
            sqlx::query("CALL DOLT_COMMIT('-m', 'apply test migrations on main')")
                .execute(&pool)
                .await
                .map_err(DbError::from)?;
        }
        Ok::<(), DbError>(())
    }
    .await;
    pool.close().await;
    result
}

async fn create_test_branch(
    server_prefix: &str,
    shared_db: &str,
    branch: &str,
) -> DbResult<()> {
    use sqlx::{ConnectOptions, Connection, Executor};
    let url = format!("{server_prefix}/{shared_db}");
    let opts = MySqlConnectOptions::from_str(&url)
        .map_err(|e| DbError::InvalidData(format!("invalid mysql url: {e}")))?;
    let mut conn = opts.connect().await.map_err(DbError::from)?;
    let stmt = format!("CALL DOLT_BRANCH('{branch}', 'main')");
    conn.execute(stmt.as_str()).await.map_err(DbError::from)?;
    conn.close().await.map_err(DbError::from)?;
    Ok(())
}

/// Clone `djinn_test_template` into a freshly-created per-test database.
///
/// `CREATE TABLE ... LIKE` would be simpler, but it does not copy FK
/// constraints — and several repositories rely on `ON DELETE CASCADE`
/// (tasks→projects, blockers→tasks, notes→projects, embeddings→notes,
/// task_runs→tasks, etc.). Replaying each table's `SHOW CREATE TABLE`
/// preserves indexes, FK constraints, FULLTEXT keys, defaults, and
/// engine/charset settings exactly. `FOREIGN_KEY_CHECKS=0` is set on the
/// admin connection for the duration so table order does not matter.
async fn clone_mysql_test_template(server_prefix: &str, test_db: &str) -> DbResult<()> {
    if !is_safe_database_identifier(test_db) {
        return Err(DbError::InvalidData(format!(
            "unsafe mysql database name `{test_db}`; only [A-Za-z0-9_] allowed",
        )));
    }

    // Connect to the system `mysql` schema with a SINGLE connection,
    // not a pool. We need `SET SESSION FOREIGN_KEY_CHECKS=0` to apply
    // to every subsequent statement in this clone, and a pool would
    // round-robin across connections that don't carry the SET. Using
    // `MySqlConnection` (single conn) makes the session state stable.
    use sqlx::ConnectOptions;
    let admin_url = format!("{server_prefix}/mysql");
    let opts = sqlx::mysql::MySqlConnectOptions::from_str(&admin_url)
        .map_err(DbError::from)?;
    let mut conn = opts.connect().await.map_err(DbError::from)?;

    let result: DbResult<()> = async {
        // Disable FK checks so CREATE TABLE statements can reference
        // tables that don't exist yet (the natural ordering of
        // SHOW CREATE TABLE output is by name, not by FK dependency
        // graph). FK enforcement is restored implicitly when this
        // connection drops — the per-test pool reconnects with the
        // server default (FOREIGN_KEY_CHECKS=1) so production code
        // paths see fully-enforced FKs.
        sqlx::query("SET SESSION FOREIGN_KEY_CHECKS=0")
            .execute(&mut conn)
            .await
            .map_err(DbError::from)?;

        sqlx::query(format!("CREATE DATABASE `{test_db}`").as_str())
            .execute(&mut conn)
            .await
            .map_err(DbError::from)?;

        // Enumerate template tables (and views, though we expect none).
        // MySQL 8 returns information_schema string columns as VARBINARY
        // under utf8mb4_0900_ai_ci collation; sqlx will not decode that
        // into `String`. CAST to CHAR makes the wire type unambiguous and
        // is a no-op on the round-trip.
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT CAST(TABLE_NAME AS CHAR) \
               FROM information_schema.tables \
              WHERE TABLE_SCHEMA = 'djinn_test_template' \
                AND TABLE_TYPE   = 'BASE TABLE'",
        )
        .fetch_all(&mut conn)
        .await
        .map_err(DbError::from)?;

        for t in &tables {
            // SHOW CREATE TABLE returns (Name, Create Table) on row 0.
            // MySQL 8 returns the second column as VARBINARY (not VARCHAR)
            // under the default utf8mb4_0900_ai_ci collation — sqlx is
            // strict about that, so decode as `Vec<u8>` then parse UTF-8.
            // The DDL string is always valid UTF-8.
            let row: (String, Vec<u8>) = sqlx::query_as(&format!(
                "SHOW CREATE TABLE `djinn_test_template`.`{t}`"
            ))
            .fetch_one(&mut conn)
            .await
            .map_err(DbError::from)?;
            let mut ddl = String::from_utf8(row.1).map_err(|e| {
                DbError::InvalidData(format!(
                    "SHOW CREATE TABLE returned non-UTF8 bytes for `{t}`: {e}"
                ))
            })?;
            // Rewrite `CREATE TABLE \`t\` (...)` to target the per-test
            // schema. `SHOW CREATE TABLE` does not include the schema
            // qualifier so a plain prefix substitution is safe.
            let needle = format!("CREATE TABLE `{t}`");
            let replacement = format!("CREATE TABLE `{test_db}`.`{t}`");
            ddl = ddl.replacen(&needle, &replacement, 1);
            sqlx::query(ddl.as_str())
                .execute(&mut conn)
                .await
                .map_err(DbError::from)?;

            // Copy data. Only `_sqlx_migrations` has rows in the
            // template (the migrator records its own runs there); the
            // other tables ship empty. We can't use `SELECT *` because
            // some tables have GENERATED columns (e.g. `agents.default_key`
            // which is VIRTUAL) — MySQL rejects any attempt to bind a value
            // for those, even via SELECT *. Enumerate real columns from
            // `information_schema.columns`, skipping anything with `extra`
            // containing 'GENERATED'.
            let real_cols: Vec<String> = sqlx::query_scalar(
                "SELECT CAST(COLUMN_NAME AS CHAR) \
                   FROM information_schema.columns \
                  WHERE TABLE_SCHEMA = 'djinn_test_template' \
                    AND TABLE_NAME   = ? \
                    AND extra NOT LIKE '%GENERATED%' \
                  ORDER BY ORDINAL_POSITION",
            )
            .bind(t)
            .fetch_all(&mut conn)
            .await
            .map_err(DbError::from)?;
            let col_list = real_cols
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let insert = format!(
                "INSERT INTO `{test_db}`.`{t}` ({col_list}) \
                 SELECT {col_list} FROM `djinn_test_template`.`{t}`"
            );
            sqlx::query(insert.as_str())
                .execute(&mut conn)
                .await
                .map_err(DbError::from)?;
        }
        // Force MySQL to invalidate any cached table-metadata that may
        // reference the prior-test version of `djinn_test_<uuid>` tables.
        // Without this, subsequent FK-cascade DELETEs on the per-test pool
        // hit error 1615 ("Prepared statement needs to be re-prepared")
        // because the server-side dictionary still points at indexes from
        // a DROPped sibling test's tables of the same name.
        //
        // NOTE: `FLUSH TABLES FROM <db>` is *not* valid MySQL 8 syntax —
        // it has to be `FLUSH TABLES <db>.<t1>, <db>.<t2>, ...`. We have
        // the table list in hand from the loop above so build the
        // qualified form.
        if !tables.is_empty() {
            let flush_list = tables
                .iter()
                .map(|t| format!("`{test_db}`.`{t}`"))
                .collect::<Vec<_>>()
                .join(", ");
            sqlx::query(&format!("FLUSH TABLES {flush_list}"))
                .execute(&mut conn)
                .await
                .map_err(DbError::from)?;
        }
        Ok(())
    }
    .await;

    drop(conn);
    result
}

fn is_safe_database_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Workspace-local tempdir helper retained for tests that still stage
/// on-disk fixtures (project filesystems, worktree mirrors, etc.).
pub(crate) fn test_tempdir() -> DbResult<tempfile::TempDir> {
    let base = workspace_test_tmp_dir()?;
    tempfile::Builder::new()
        .prefix("djinn-test-")
        .tempdir_in(base)
        .map_err(|e| DbError::InvalidData(e.to_string()))
}

fn workspace_test_tmp_dir() -> DbResult<std::path::PathBuf> {
    use std::path::PathBuf;

    // Prefer an explicit override for constrained CI/dev environments.
    if let Some(override_dir) = std::env::var_os("DJINN_TEST_TMPDIR") {
        let path = PathBuf::from(override_dir);
        std::fs::create_dir_all(&path).map_err(|e| DbError::InvalidData(e.to_string()))?;
        return Ok(path);
    }

    // Otherwise place test tempdirs under the workspace root when discoverable.
    if let Some(base) = workspace_root_from_current_dir() {
        let candidate = base.join("target").join("test-tmp");
        std::fs::create_dir_all(&candidate).map_err(|e| DbError::InvalidData(e.to_string()))?;
        return Ok(candidate);
    }

    // Final fallback: root under the current crate's target directory.
    let current_dir = std::env::current_dir().map_err(|e| DbError::InvalidData(e.to_string()))?;
    let fallback = current_dir.join("target").join("test-tmp");
    std::fs::create_dir_all(&fallback).map_err(|e| DbError::InvalidData(e.to_string()))?;
    Ok(fallback)
}

fn workspace_root_from_current_dir() -> Option<std::path::PathBuf> {
    let mut current = std::env::current_dir().ok()?;

    loop {
        let candidate = current.join("Cargo.lock");
        if candidate.exists() {
            return Some(current);
        }

        if !current.pop() {
            return None;
        }
    }
}

/// Legacy vector-extension status. The sqlite-vec backend is retired; this
/// type is retained for call-site compatibility and always reports
/// `available = false` now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqliteVecStatus {
    pub available: bool,
    pub version: Option<String>,
    pub detail: Option<String>,
}

#[cfg(test)]
pub(crate) fn set_sqlite_vec_disabled_for_tests(_disabled: bool) {
    // No-op: sqlite-vec is retired. Retained as a shim so embedding tests
    // compile pending their rewrite against Qdrant.
}

/// Default database path: `~/.djinn/djinn.db`.
///
/// Retained for CLI-plumbing backward compatibility even though the
/// SQLite backend is retired; external callers may still consult this
/// when they have a historical path to reason about.
pub fn default_db_path() -> std::path::PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".djinn")
        .join("djinn.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mysql_backend_selection_metadata_is_dolt_shaped() {
        let db = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3306/djinn".to_owned(),
            flavor: MysqlBackendFlavor::Dolt,
        }))
        .expect("mysql/dolt backend should construct a concrete runtime path");

        assert_eq!(db.backend_kind(), DatabaseBackendKind::Mysql);
        assert_eq!(db.bootstrap_info().backend_label, "dolt");
        assert_eq!(
            db.backend_capabilities().lexical_search,
            NoteSearchBackend::MysqlFulltext
        );
        assert!(db.backend_capabilities().supports_branching_metadata);
        assert!(!db.backend_capabilities().supports_sqlite_vec);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_schema_is_current_passes_on_a_freshly_migrated_db() {
        // `open_in_memory` runs the migrator as part of `ensure_initialized`,
        // so once it succeeds we should be at HEAD.
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        db.verify_schema_is_current()
            .await
            .expect("verify_schema_is_current should pass on a freshly migrated DB");
    }

    // The "no `_sqlx_migrations` table at all" branch of
    // `verify_schema_is_current` is exercised in production by a brand-new
    // empty database. It can't be unit-tested against `open_in_memory`
    // because that fixture inherits a fully-migrated `main` branch and
    // `_sqlx_migrations` is always present after fork. Covered by the
    // production error string + the `_fails_when_a_migration_is_missing`
    // case below.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_schema_is_current_fails_when_a_migration_is_missing() {
        // Migrate to HEAD, then delete the most recent row to fake a
        // binary that ships one migration the DB has not received yet.
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();

        // Drop the newest applied version. The binary's compile-time
        // migrator still expects it, so verify must flag the gap.
        sqlx::query(
            "DELETE FROM _sqlx_migrations \
             WHERE version = (SELECT * FROM (SELECT MAX(version) FROM _sqlx_migrations) AS m)",
        )
        .execute(db.pool())
        .await
        .expect("drop newest migration row");

        let err = db
            .verify_schema_is_current()
            .await
            .expect_err("verify should refuse when binary-known migrations are missing");
        let message = err.to_string();
        assert!(
            message.contains("missing migrations"),
            "expected error to call out the missing version list, got: {message}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_in_memory_generates_unique_isolation_containers() {
        let a = Database::open_in_memory().unwrap();
        let b = Database::open_in_memory().unwrap();
        // The Dolt variant gives each test a distinct branch; the MySQL
        // variant gives each test a distinct database. Either way, two
        // back-to-back `open_in_memory` calls produce different isolation
        // identifiers.
        let a_init = a
            .test_branch
            .as_ref()
            .expect("open_in_memory sets test_branch");
        let b_init = b
            .test_branch
            .as_ref()
            .expect("open_in_memory sets test_branch");
        match (a_init, b_init) {
            (
                TestBranchInit::Dolt {
                    shared_db: a_db,
                    branch: a_br,
                    ..
                },
                TestBranchInit::Dolt {
                    branch: b_br, ..
                },
            ) => {
                assert_ne!(a_br, b_br);
                assert!(a_br.starts_with("t_"));
                assert_eq!(a_db, "djinn_test");
                assert_eq!(a.bootstrap_info().target, b.bootstrap_info().target);
            }
            (
                TestBranchInit::Mysql { test_db: a_db, .. },
                TestBranchInit::Mysql { test_db: b_db, .. },
            ) => {
                assert_ne!(a_db, b_db);
                assert!(a_db.starts_with("djinn_test_"));
                assert_ne!(a.bootstrap_info().target, b.bootstrap_info().target);
            }
            (left, right) => panic!(
                "open_in_memory should pick one flavor per process, got {left:?} vs {right:?}"
            ),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mysql_and_dolt_capabilities_differ_only_by_branch_metadata() {
        let mysql = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3306/djinn".to_owned(),
            flavor: MysqlBackendFlavor::Mysql,
        }))
        .unwrap();
        let dolt = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3307/djinn".to_owned(),
            flavor: MysqlBackendFlavor::Dolt,
        }))
        .unwrap();

        assert_eq!(
            mysql.backend_capabilities().lexical_search,
            dolt.backend_capabilities().lexical_search
        );
        assert_eq!(
            mysql.backend_capabilities().note_vector_backend,
            dolt.backend_capabilities().note_vector_backend
        );
        assert!(!mysql.backend_capabilities().supports_branching_metadata);
        assert!(dolt.backend_capabilities().supports_branching_metadata);
    }
}
