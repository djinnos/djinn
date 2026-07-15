//! Migration 119 — `retrieval_traces` `rollout_label` + trace-level `outcome`
//! columns and deterministic historical classifier for epic `u9hc` /
//! proposal `01mm`.
//!
//! Verifies the new migration:
//!   * applies cleanly on a fresh database (via `sqlx::migrate!`),
//!   * applies additively on top of the V1..V118 chain (prior-schema path),
//!   * installs non-null `rollout_label` and `outcome` columns with the
//!     documented CHECK constraint and a conservative default,
//!   * installs the project/time grouping indexes,
//!   * backfills historical rows with a conservative deterministic classifier
//!     that only assigns `injected` when `estimated_injected_tokens > 0` and a
//!     well-formed candidate has JSON-null `skipped_reason`, only assigns
//!     `empty` for reliable well-formed zero-injection evidence, and leaves
//!     absent / malformed / contradictory evidence as `legacy_unknown`,
//!   * keeps existing writers schema-compatible through truthful defaults
//!     (a future legacy insert without `rollout_label`/`outcome` reads as
//!     `rollout_label = 'legacy'`, `outcome = 'legacy_unknown'`, never
//!     `injected`).
//!
//! Mirrors the harness pattern in `migrations_liveness_evidence_outcomes.rs`
//! (migration 95) so this test fits the existing infra without inventing a new
//! pattern.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 119;
const MIGRATION_FILE: &str = "119_retrieval_traces_rollout_outcome.sql";

fn base_database_url() -> String {
    std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .unwrap_or_else(|_| {
            "postgres://djinn:VipjO1uAdxAGvNSA6EcJdZMdHAquYeJj@djinn-postgres.djinn.svc.cluster.local:5432/djinn"
                .to_owned()
        })
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
    let mut entries: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .map(|entry| {
            let path = entry.expect("migration dir entry").path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let version = name
                .split_once('_')
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| path.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    entries.sort_by(|(av, ap), (bv, bp)| {
        av.cmp(bv).then_with(|| {
            ap.file_name()
                .unwrap_or_default()
                .cmp(bp.file_name().unwrap_or_default())
        })
    });
    entries
}

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let db_name = format!(
        "djinn_migration_{}_{}",
        suffix,
        uuid::Uuid::now_v7().simple()
    );
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
    let result = f(db_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{db_name}""#).as_str())
        .await
        .expect("drop migration test database");

    result
}

/// Apply every migration whose version prefix is strictly less than
/// `MIGRATION_VERSION`. This is the "prior migrations" path used to verify
/// that migration 119 is additive on top of the entire V1..V118 chain.
async fn apply_prior_migrations(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        if version == 0 {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration sql");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
    }
}

async fn apply_migration_119(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 119 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 119 after prior migrations");
}

/// Seed a project so retrieval_traces rows can reference it.
async fn seed_project(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('project-rt', 'project-rt', 'djinnos', 'djinn-rt') \
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed project");
}

/// Assert the migration installed the expected columns, CHECK constraint, and
/// indexes. Shared by the fresh-database and prior-schema paths so any drift is
/// caught in both.
async fn assert_retrieval_traces_schema(pool: &sqlx::PgPool) {
    // ── rollout_label column ──────────────────────────────────────────────
    let (rl_type, rl_nullable, rl_default): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_name = 'retrieval_traces' AND column_name = 'rollout_label'",
        )
        .fetch_one(pool)
        .await
        .expect("inspect retrieval_traces.rollout_label");
    assert_eq!(
        rl_type.as_deref(),
        Some("character varying"),
        "rollout_label should be character varying, got {rl_type:?}"
    );
    assert_eq!(
        rl_nullable.as_deref(),
        Some("NO"),
        "rollout_label should be NOT NULL, got {rl_nullable:?}"
    );
    assert_eq!(
        rl_default.as_deref(),
        Some("'legacy'::character varying"),
        "rollout_label default should be 'legacy', got {rl_default:?}"
    );

    // ── outcome column ────────────────────────────────────────────────────
    let (oc_type, oc_nullable, oc_default): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_name = 'retrieval_traces' AND column_name = 'outcome'",
        )
        .fetch_one(pool)
        .await
        .expect("inspect retrieval_traces.outcome");
    assert_eq!(
        oc_type.as_deref(),
        Some("character varying"),
        "outcome should be character varying, got {oc_type:?}"
    );
    assert_eq!(
        oc_nullable.as_deref(),
        Some("NO"),
        "outcome should be NOT NULL, got {oc_nullable:?}"
    );
    assert_eq!(
        oc_default.as_deref(),
        Some("'legacy_unknown'::character varying"),
        "outcome default should be 'legacy_unknown', got {oc_default:?}"
    );

    // ── outcome CHECK constraint accepts the seven required values ────────
    let outcome_check: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) \
         FROM pg_constraint \
         WHERE conrelid = 'retrieval_traces'::regclass \
           AND contype = 'c' \
           AND conname = 'retrieval_traces_outcome_check'",
    )
    .fetch_optional(pool)
    .await
    .expect("inspect retrieval_traces_outcome_check");
    let body = outcome_check.unwrap_or_default();
    for value in [
        "injected",
        "empty",
        "error",
        "legacy_unknown",
        "disabled_off",
        "disabled_kill_switch",
        "disabled_legacy",
    ] {
        assert!(
            body.contains(&format!("'{value}'")),
            "retrieval_traces_outcome_check should accept '{value}', got body: {body}"
        );
    }

    // ── project/time grouping indexes exist ──────────────────────────────
    let index_names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_index i ON i.indexrelid = c.oid \
         JOIN pg_class t ON t.oid = i.indrelid \
         WHERE t.relname = 'retrieval_traces' \
           AND c.relname IN ( \
             'idx_retrieval_traces_project_entry_rollout_outcome_created', \
             'idx_retrieval_traces_project_rollout_created', \
             'idx_retrieval_traces_project_outcome_created' \
           ) \
         ORDER BY c.relname",
    )
    .fetch_all(pool)
    .await
    .expect("inspect retrieval_traces grouping indexes");
    let mut expected = vec![
        "idx_retrieval_traces_project_entry_rollout_outcome_created",
        "idx_retrieval_traces_project_outcome_created",
        "idx_retrieval_traces_project_rollout_created",
    ];
    expected.sort();
    assert_eq!(
        index_names, expected,
        "expected grouping indexes, got {index_names:?}"
    );
}

// ── Historical backfill fixtures ──────────────────────────────────────────────
//
// Each fixture inserts a retrieval_traces row BEFORE migration 119 applies, so
// the classifier backfill is exercised against pre-migration evidence. The
// `(id, expected_outcome)` tuples encode the conservative classification each
// row must receive after the migration runs.

/// Insert a single retrieval_traces row with explicit candidates JSONB and
/// estimated_injected_tokens. `candidates_json` is the raw JSON string stored
/// verbatim so malformed shapes are represented exactly.
async fn seed_trace(
    pool: &sqlx::PgPool,
    id: &str,
    estimated_injected_tokens: i32,
    candidates_json: &str,
) {
    sqlx::query(
        "INSERT INTO retrieval_traces \
             (id, schema_version, project_id, entry_point, candidates, estimated_injected_tokens) \
         VALUES ($1, 1, 'project-rt', 'dispatch', $2::jsonb, $3)",
    )
    .bind(id)
    .bind(candidates_json)
    .bind(estimated_injected_tokens)
    .execute(pool)
    .await
    .unwrap_or_else(|e| panic!("seed retrieval_traces row {id}: {e}"));
}

/// Historical backfill fixtures. Each `(id, candidates_json, tokens,
/// expected_outcome)` tuple is a pre-migration row whose classification the
/// migration must reproduce deterministically.
const HISTORICAL_FIXTURES: &[(&str, &str, i32, &str)] = &[
    // ── injected: positive tokens + well-formed candidate with JSON-null
    //    skipped_reason. ────────────────────────────────────────────────────
    (
        "rt-injected-ok",
        r#"[{"note_id":"n1","outcome":"injected","skipped_reason":null,"rank":1,"confidence":0.9}]"#,
        120,
        "injected",
    ),
    // injected even when the injected candidate is mixed with skipped ones.
    (
        "rt-injected-mixed",
        r#"[{"note_id":"n1","outcome":"injected","skipped_reason":null,"rank":1,"confidence":0.9},{"note_id":"n2","outcome":"skipped","skipped_reason":"not_top_k","rank":2,"confidence":0.1}]"#,
        120,
        "injected",
    ),
    // ── empty: zero tokens + well-formed array with NO injected candidate
    //    (no candidate whose skipped_reason is JSON-null). The empty array is
    //    reliable zero-injection evidence. ──────────────────────────────────
    ("rt-empty-array", r#"[]"#, 0, "empty"),
    // empty with skipped candidates only (no JSON-null skipped_reason).
    (
        "rt-empty-skipped-only",
        r#"[{"note_id":"n1","outcome":"skipped","skipped_reason":"not_top_k","rank":1,"confidence":0.1}]"#,
        0,
        "empty",
    ),
    // ── legacy_unknown: absent evidence (candidates default is '[]' but we
    //    also exercise the default path by relying on column default when no
    //    explicit payload proves injection). Here tokens > 0 but no injected
    //    candidate — contradictory evidence. ────────────────────────────────
    (
        "rt-tokens-no-injected-cand",
        r#"[{"note_id":"n1","outcome":"skipped","skipped_reason":"not_top_k","rank":1,"confidence":0.1}]"#,
        50,
        "legacy_unknown",
    ),
    // ── legacy_unknown: candidate has skipped_reason present (non-null) but
    //    tokens > 0. There is no JSON-null skipped_reason, so the injected
    //    predicate fails; the empty predicate fails because tokens > 0.
    //    Contradictory. ─────────────────────────────────────────────────────
    (
        "rt-injected-cand-with-reason",
        r#"[{"note_id":"n1","outcome":"injected","skipped_reason":"not_top_k","rank":1,"confidence":0.9}]"#,
        120,
        "legacy_unknown",
    ),
    // ── legacy_unknown: malformed candidates (object instead of array). The
    //    predicate must inspect jsonb_typeof before expanding elements, so
    //    this must NOT raise and must NOT be optimistically classified. ──────
    (
        "rt-malformed-object",
        r#"{"note_id":"n1","outcome":"injected","skipped_reason":null}"#,
        120,
        "legacy_unknown",
    ),
    // ── legacy_unknown: malformed candidates (scalar string). ──────────────
    (
        "rt-malformed-scalar",
        r#""not-an-array""#,
        120,
        "legacy_unknown",
    ),
    // ── legacy_unknown: candidate is not a JSON object (it's a string inside
    //    the array). The predicate filters on jsonb_typeof = 'object'. ───────
    (
        "rt-malformed-elem-string",
        r#"["not-an-object"]"#,
        120,
        "legacy_unknown",
    ),
    // ── legacy_unknown: zero tokens but candidates is an object (non-array),
    //    so reliable zero-injection evidence is absent. ─────────────────────
    (
        "rt-zero-tokens-non-array",
        r#"{"outcome":"skipped"}"#,
        0,
        "legacy_unknown",
    ),
    // ── injected but tokens == 0: contradictory — cannot be injected with
    //    zero injected tokens even if a candidate has JSON-null
    //    skipped_reason. ────────────────────────────────────────────────────
    (
        "rt-injected-cand-zero-tokens",
        r#"[{"note_id":"n1","outcome":"injected","skipped_reason":null,"rank":1,"confidence":0.9}]"#,
        0,
        "legacy_unknown",
    ),
    // ── legacy_unknown: candidate object lacks skipped_reason key entirely.
    //    The JSON-null test (cand -> 'skipped_reason') = 'null'::jsonb is
    //    FALSE for a missing key (it returns SQL NULL on the left side),
    //    so the injected predicate fails. ───────────────────────────────────
    (
        "rt-injected-cand-missing-key",
        r#"[{"note_id":"n1","outcome":"injected","rank":1,"confidence":0.9}]"#,
        120,
        "legacy_unknown",
    ),
];

/// Assert every historical fixture row received its expected outcome after the
/// migration backfill, and that rollout_label defaulted to 'legacy'.
async fn assert_historical_backfill(pool: &sqlx::PgPool) {
    for (id, _candidates_json, _tokens, expected_outcome) in HISTORICAL_FIXTURES {
        let (rollout_label, outcome): (String, String) =
            sqlx::query_as("SELECT rollout_label, outcome FROM retrieval_traces WHERE id = $1")
                .bind(*id)
                .fetch_one(pool)
                .await
                .unwrap_or_else(|e| panic!("load backfilled row {id}: {e}"));
        assert_eq!(
            rollout_label, "legacy",
            "historical row {id} rollout_label should default to 'legacy', got {rollout_label:?}"
        );
        assert_eq!(
            outcome, *expected_outcome,
            "historical row {id} outcome should be {expected_outcome}, got {outcome:?}"
        );
    }
}

/// Assert that a future legacy insert (one that does NOT set rollout_label or
/// outcome) reads back with the conservative truthful defaults and never as
/// injected. This proves existing writers remain schema-compatible.
async fn assert_future_legacy_insert_is_truthful(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO retrieval_traces \
             (id, schema_version, project_id, entry_point, candidates, estimated_injected_tokens) \
         VALUES ('rt-future-legacy', 1, 'project-rt', 'dispatch', \
                 '[{\"note_id\":\"n1\",\"outcome\":\"injected\",\"skipped_reason\":null}]'::jsonb, \
                 999)",
    )
    .execute(pool)
    .await
    .expect("insert future legacy row");

    let (rollout_label, outcome): (String, String) = sqlx::query_as(
        "SELECT rollout_label, outcome FROM retrieval_traces WHERE id = 'rt-future-legacy'",
    )
    .fetch_one(pool)
    .await
    .expect("load future legacy row");
    assert_eq!(
        rollout_label, "legacy",
        "future legacy insert rollout_label should default to 'legacy', got {rollout_label:?}"
    );
    // The default must be legacy_unknown, NOT injected, even though the row
    // carries an injected candidate. The classifier only runs at migration
    // time; new rows use the column default.
    assert_eq!(
        outcome, "legacy_unknown",
        "future legacy insert outcome should default to 'legacy_unknown' (truthful), got {outcome:?}"
    );
}

/// Assert the CHECK constraint rejects an out-of-vocabulary outcome.
async fn assert_outcome_check_rejects_unknown(pool: &sqlx::PgPool) {
    let result = sqlx::query(
        "INSERT INTO retrieval_traces \
             (id, schema_version, project_id, entry_point, candidates, estimated_injected_tokens, outcome) \
         VALUES ('rt-bad-outcome', 1, 'project-rt', 'dispatch', '[]'::jsonb, 0, 'bogus_outcome')",
    )
    .execute(pool)
    .await;
    assert!(
        result.is_err(),
        "insert with out-of-vocabulary outcome should be rejected by the CHECK constraint"
    );
}

/// Assert all seven outcome values are accepted by the CHECK constraint.
async fn assert_all_outcomes_accepted(pool: &sqlx::PgPool) {
    for (idx, outcome) in [
        "injected",
        "empty",
        "error",
        "legacy_unknown",
        "disabled_off",
        "disabled_kill_switch",
        "disabled_legacy",
    ]
    .iter()
    .enumerate()
    {
        let id = format!("rt-vocab-{idx}");
        sqlx::query(
            "INSERT INTO retrieval_traces \
                 (id, schema_version, project_id, entry_point, candidates, estimated_injected_tokens, outcome) \
             VALUES ($1, 1, 'project-rt', 'dispatch', '[]'::jsonb, 0, $2)",
        )
        .bind(&id)
        .bind(*outcome)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("insert with outcome '{outcome}' should be accepted: {e}"));
    }
}

/// Assert verbatim rollout labels (including cohort: and legacy labels) survive
/// a round-trip without being collapsed.
async fn assert_rollout_label_verbatim(pool: &sqlx::PgPool) {
    for label in [
        "enabled",
        "off",
        "kill_switch",
        "cohort:alpha",
        "cohort:experiment-42",
        "legacy",
    ] {
        let id = format!("rt-label-{}", label.replace(':', "-"));
        sqlx::query(
            "INSERT INTO retrieval_traces \
                 (id, schema_version, project_id, entry_point, candidates, estimated_injected_tokens, rollout_label) \
             VALUES ($1, 1, 'project-rt', 'dispatch', '[]'::jsonb, 0, $2)",
        )
        .bind(&id)
        .bind(label)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("insert rollout_label '{label}': {e}"));

        let stored: String =
            sqlx::query_scalar("SELECT rollout_label FROM retrieval_traces WHERE id = $1")
                .bind(&id)
                .fetch_one(pool)
                .await
                .expect("load verbatim rollout label");
        assert_eq!(
            stored, *label,
            "rollout_label '{label}' should be stored verbatim, got {stored:?}"
        );
    }
}

/// Seed all historical fixtures into a pre-migration-119 database.
async fn seed_historical_fixtures(pool: &sqlx::PgPool) {
    seed_project(pool).await;
    for (id, candidates_json, tokens, _expected) in HISTORICAL_FIXTURES {
        seed_trace(pool, id, *tokens, candidates_json).await;
    }
}

#[tokio::test]
async fn migration_119_applies_on_fresh_database() {
    with_temp_database("fresh_rt_rollout", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        sqlx::migrate!("./migrations_postgres")
            .run(&pool)
            .await
            .expect("apply all migrations to fresh database");

        assert_retrieval_traces_schema(&pool).await;
        seed_project(&pool).await;
        assert_future_legacy_insert_is_truthful(&pool).await;
        assert_outcome_check_rejects_unknown(&pool).await;
        assert_all_outcomes_accepted(&pool).await;
        assert_rollout_label_verbatim(&pool).await;

        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_119_applies_after_prior_migrations() {
    with_temp_database("prior_rt_rollout", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;

        // Seed historical fixtures BEFORE migration 119 applies, so the
        // deterministic backfill classifier is exercised against pre-migration
        // evidence.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect prior migration database (pool)");
        seed_historical_fixtures(&pool).await;
        pool.close().await;

        apply_migration_119(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");

        assert_retrieval_traces_schema(&pool).await;
        assert_historical_backfill(&pool).await;

        pool.close().await;
    })
    .await;
}
