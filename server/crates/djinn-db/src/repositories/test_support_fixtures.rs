//! Larger multi-row fixtures for `test_support`.
//!
//! Split out of `test_support.rs`, which sits at the 51200-byte `Server Guards`
//! ceiling — close enough that running the project's own formatter over it
//! pushes it past. Pure code motion; every item is re-exported through
//! `test_support`, so no import path changes.

use std::path::PathBuf;

use djinn_core::events::EventBus;
use djinn_core::models::{Project, SessionRecord};
use tokio::sync::broadcast;

use crate::database::Database;
use crate::repositories::note::NoteRepository;
use crate::repositories::session::{CreateSessionParams, SessionRepository};
use crate::repositories::test_support::{event_bus_for, make_project, seed_test_user};
use djinn_memory::Note;

/// Persisted GitHub coordinates and installation identity for coordinator
/// provider fixtures. **Not for production use.**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectGithubInstallationForTest {
    pub owner: String,
    pub repo: String,
    pub installation_id: u64,
}

/// Give an existing fixture project a real GitHub installation identity.
///
/// This keeps coordinator tests out of raw SQL while exercising the production
/// project-repository installation lookup. Panics on fixture SQL errors.
pub async fn persist_project_github_installation_for_test(
    db: &Database,
    project_id: &str,
    owner: &str,
    repo: &str,
    installation_id: u64,
) -> ProjectGithubInstallationForTest {
    db.ensure_initialized().await.unwrap();
    assert!(
        installation_id <= i64::MAX as u64,
        "installation id must fit SQL i64"
    );
    let updated = sqlx::query(
        "UPDATE projects SET github_owner = $2, github_repo = $3, installation_id = $4 WHERE id = $1",
    )
    .bind(project_id)
    .bind(owner)
    .bind(repo)
    .bind(installation_id as i64)
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(updated.rows_affected(), 1, "fixture project must exist");
    ProjectGithubInstallationForTest {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        installation_id,
    }
}

/// Seed the canonical model-turn admission rows used by slot-boundary tests.
pub async fn seed_model_turn_admission_fixture(
    db: &Database,
    phase: &str,
    capability: &str,
    available: i64,
) -> i64 {
    db.ensure_initialized().await.unwrap();
    sqlx::query("INSERT INTO credentials (id, provider_id, key_name, encrypted_value) VALUES ('credential-slot', 'provider', 'key-slot', decode('00', 'hex'))")
        .execute(db.pool()).await.unwrap();
    let pool = sqlx::query_scalar("INSERT INTO model_turn_pools (credential_id, provider_id, model_id, phase, capability_state, learned_concurrency) VALUES ('credential-slot', 'provider', 'model', $1, $2, 1) RETURNING id")
        .bind(phase).bind(capability).fetch_one(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO model_turn_bucket_bindings (pool_id, bucket_kind, capacity_units, available_units) VALUES ($1, 'request', 2, $2)")
        .bind(pool).bind(available).execute(db.pool()).await.unwrap();
    pool
}

/// Seed a credential and its exact provider/model-scoped admission pool.
///
/// This test-only seam is provider-neutral: callers pass the identity and
/// scope produced by the provider attempt plan they are exercising.
pub async fn seed_scoped_model_turn_admission_fixture(
    db: &Database,
    credential_id: &str,
    provider_id: &str,
    model_id: &str,
    phase: &str,
    capability: &str,
    learned_concurrency: i64,
) -> i64 {
    assert!(
        learned_concurrency > 0,
        "fixture learned concurrency must be positive"
    );
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO credentials (id, provider_id, key_name, encrypted_value) \
         VALUES ($1, $2, $3, decode('00', 'hex'))",
    )
    .bind(credential_id)
    .bind(provider_id)
    .bind(format!("model-turn-admission-fixture-{credential_id}"))
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO model_turn_pools \
         (credential_id, provider_id, model_id, phase, capability_state, learned_concurrency) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(credential_id)
    .bind(provider_id)
    .bind(model_id)
    .bind(phase)
    .bind(capability)
    .bind(learned_concurrency)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

pub async fn model_turn_decision_fixture(db: &Database, pool_id: i64) -> (String, Option<String>) {
    sqlx::query_as(
        "SELECT request_fingerprint, diagnostic FROM model_turn_decisions WHERE pool_id = $1",
    )
    .bind(pool_id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

pub async fn model_turn_decision_count_fixture(db: &Database, pool_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM model_turn_decisions WHERE pool_id = $1")
        .bind(pool_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

pub async fn model_turn_accounting_fixture(db: &Database, pool_id: i64) -> (i64, i64, i64) {
    sqlx::query_as("SELECT p.in_flight, b.available_units, b.quarantined_units FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id WHERE p.id = $1")
        .bind(pool_id).fetch_one(db.pool()).await.unwrap()
}

pub async fn set_model_turn_capability_fixture(db: &Database, pool_id: i64, capability: &str) {
    sqlx::query("UPDATE model_turn_pools SET capability_state = $2 WHERE id = $1")
        .bind(pool_id)
        .bind(capability)
        .execute(db.pool())
        .await
        .unwrap();
}

/// Set a pool phase after fixture setup to exercise a real admission branch.
pub async fn set_model_turn_phase_fixture(db: &Database, pool_id: i64, phase: &str) {
    sqlx::query("UPDATE model_turn_pools SET phase = $2 WHERE id = $1")
        .bind(pool_id)
        .bind(phase)
        .execute(db.pool())
        .await
        .unwrap();
}

/// Count every persisted `model_turn_leases` row in the database.
///
/// The conformance target asserts admission outcomes by the number of durable
/// lease rows the production acquisition path actually wrote, rather than by
/// the enum a fixture handed back. A denied acquisition must leave this total
/// unchanged.
pub async fn model_turn_lease_total_count_fixture(db: &Database) -> i64 {
    db.ensure_initialized().await.unwrap();
    sqlx::query_scalar("SELECT count(*) FROM model_turn_leases")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Count persisted `model_turn_leases` rows scoped to one admission pool.
pub async fn model_turn_lease_count_for_pool_fixture(db: &Database, pool_id: i64) -> i64 {
    db.ensure_initialized().await.unwrap();
    sqlx::query_scalar("SELECT count(*) FROM model_turn_leases WHERE pool_id = $1")
        .bind(pool_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// The largest `model_turn_pools.id` present, or `None` for an empty ledger.
///
/// Lets a test derive a pool id that provably does not resolve without
/// hard-coding one that a parallel fixture might later occupy.
pub async fn model_turn_max_pool_id_fixture(db: &Database) -> Option<i64> {
    db.ensure_initialized().await.unwrap();
    sqlx::query_scalar("SELECT max(id) FROM model_turn_pools")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

pub async fn model_turn_lease_lifecycle_fixture(db: &Database, lease_id: &str) -> String {
    sqlx::query_scalar("SELECT lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid")
        .bind(lease_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Return the durable slot owner for a particular fenced lease.
///
/// This reads the repository row rather than reusing an acquisition request so
/// cross-slot capability tests prove the persisted ownership boundary.
pub async fn model_turn_lease_owner_pod_uid_fixture(
    db: &Database,
    lease_id: &str,
) -> Option<String> {
    sqlx::query_scalar("SELECT owner_pod_uid FROM model_turn_leases WHERE lease_id = $1::uuid")
        .bind(lease_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Return persisted provider-launch identities in generation order.
pub async fn model_turn_launch_identities_fixture(db: &Database) -> Vec<(String, i64, String)> {
    sqlx::query_as(
        "SELECT lease_id::text, generation, request_id FROM model_turn_leases ORDER BY generation",
    )
    .fetch_all(db.pool())
    .await
    .unwrap()
}

/// Snapshot one lease's fenced heartbeat state for watchdog regression tests.
pub async fn model_turn_lease_heartbeat_snapshot_fixture(
    db: &Database,
    lease_id: &str,
) -> (i64, Option<String>) {
    sqlx::query_as(
        "SELECT generation, heartbeat_at::text FROM model_turn_leases WHERE lease_id = $1::uuid",
    )
    .bind(lease_id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

pub async fn model_turn_request_lifecycle_fixture(db: &Database, request_id: &str) -> String {
    sqlx::query_scalar("SELECT lifecycle FROM model_turn_leases WHERE request_id = $1")
        .bind(request_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

pub async fn model_turn_terminal_fixture(
    db: &Database,
    lease_id: &str,
    generation: i64,
    request_id: &str,
) -> (String, String) {
    sqlx::query_as("SELECT outcome, accounting_state FROM model_turn_lease_terminals WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3")
        .bind(lease_id).bind(generation).bind(request_id).fetch_one(db.pool()).await.unwrap()
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixtureExpectedCounts {
    pub prune_associations: u64,
    pub flag_orphan_notes: u64,
    pub rebuild_missing_content_hashes: u64,
    pub repair_broken_wikilinks: u64,
    /// Number of notes the housekeeping tick's archive sweep is expected to
    /// flip from `active` to `archived` for this project. The shipped
    /// multi-project fixture only uses hand-written `reference` notes, so
    /// this stays at 0 in the default fixture — archive candidates live in
    /// the dedicated single-project archive aggregation test.
    pub archive_audit_candidates: u64,
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixtureProject {
    pub project: Project,
    pub path: PathBuf,
    pub expected: HousekeepingFixtureExpectedCounts,
    pub orphan_note_id: String,
    pub repaired_source_note_id: String,
    pub repaired_target_note_id: String,
    pub legacy_hash_note_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct HousekeepingFixture {
    pub projects: Vec<HousekeepingFixtureProject>,
}

pub async fn build_multi_project_housekeeping_fixture(db: &Database) -> HousekeepingFixture {
    let tmp = crate::database::test_tempdir().unwrap();
    let root = tmp.keep();
    let project_one_path = root.join("project-one");
    let project_two_path = root.join("project-two");
    std::fs::create_dir_all(&project_one_path).unwrap();
    std::fs::create_dir_all(&project_two_path).unwrap();

    let project_one = make_project(db, &project_one_path).await;
    let project_two = make_project(db, &project_two_path).await;

    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let project_one_stale_a = repo
        .create(
            &project_one.id,
            "Project One Stale A",
            "content one",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_stale_b = repo
        .create(
            &project_one.id,
            "Project One Stale B",
            "content two",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_recent_a = repo
        .create(
            &project_one.id,
            "Project One Recent A",
            "content three",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_recent_b = repo
        .create(
            &project_one.id,
            "Project One Recent B",
            "content four",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_orphan = repo
        .create(
            &project_one.id,
            "Project One Orphan",
            "orphan body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_linked_target = repo
        .create(
            &project_one.id,
            "Project One Linked Target",
            "linked body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _project_one_linked_source = repo
        .create(
            &project_one.id,
            "Project One Linked Source",
            &format!("links to [[{}]]", project_one_linked_target.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_canonical_hash = repo
        .create_db_note(
            &project_one.id,
            "Project One Canonical Hash",
            "Alpha\r\nBeta\n",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_legacy_hash = repo
        .create_db_note(
            &project_one.id,
            "Project One Legacy Hash",
            " Alpha\nBeta ",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_repair_target = repo
        .create(
            &project_one.id,
            "Rust Ownership Guide",
            "Rust Ownership. Rust Ownership. Rust Ownership. Rust Ownership. Borrowing and lifetimes details.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_one_repair_source = repo
        .create(
            &project_one.id,
            "Project One Broken Link Source",
            "Read [[Rust Ownership]] before editing.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let project_two_stale_a = repo
        .create(
            &project_two.id,
            "Project Two Stale A",
            "content five",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_stale_b = repo
        .create(
            &project_two.id,
            "Project Two Stale B",
            "content six",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_recent_a = repo
        .create(
            &project_two.id,
            "Project Two Recent A",
            "content seven",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_recent_b = repo
        .create(
            &project_two.id,
            "Project Two Recent B",
            "content eight",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_orphan = repo
        .create(
            &project_two.id,
            "Project Two Orphan",
            "orphan body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_linked_target = repo
        .create(
            &project_two.id,
            "Project Two Linked Target",
            "linked body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _project_two_linked_source = repo
        .create(
            &project_two.id,
            "Project Two Linked Source",
            &format!("links to [[{}]]", project_two_linked_target.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_canonical_hash = repo
        .create_db_note(
            &project_two.id,
            "Project Two Canonical Hash",
            "Gamma\r\nDelta\n",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_legacy_hash = repo
        .create_db_note(
            &project_two.id,
            "Project Two Legacy Hash",
            " Gamma\nDelta ",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_repair_target = repo
        .create(
            &project_two.id,
            "Async Runtime Guide",
            "Async Runtime. Async Runtime. Async Runtime. Async Runtime. Scheduling and executors details.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let project_two_repair_source = repo
        .create(
            &project_two.id,
            "Project Two Broken Link Source",
            "Review [[Async Runtime]] before tuning workers.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    repo.upsert_association(&project_one_stale_a.id, &project_one_stale_b.id, 1)
        .await
        .unwrap();
    repo.upsert_association(&project_one_recent_a.id, &project_one_recent_b.id, 6)
        .await
        .unwrap();
    repo.upsert_association(&project_two_stale_a.id, &project_two_stale_b.id, 1)
        .await
        .unwrap();
    repo.upsert_association(&project_two_recent_a.id, &project_two_recent_b.id, 6)
        .await
        .unwrap();

    sqlx::query!(
        r#"UPDATE note_associations
         SET last_co_access = to_char((now() at time zone 'utc') - interval '100 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE (note_a_id = $1 AND note_b_id = $2)
            OR (note_a_id = $3 AND note_b_id = $4)"#,
        project_one_stale_a.id,
        project_one_stale_b.id,
        project_two_stale_a.id,
        project_two_stale_b.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        r#"UPDATE note_associations
         SET last_co_access = to_char((now() at time zone 'utc') - interval '1 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
         WHERE (note_a_id = $1 AND note_b_id = $2)
            OR (note_a_id = $3 AND note_b_id = $4)"#,
        project_one_recent_a.id,
        project_one_recent_b.id,
        project_two_recent_a.id,
        project_two_recent_b.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        r#"UPDATE notes
         SET last_accessed = to_char((now() at time zone 'utc') - interval '31 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), access_count = 0
         WHERE id IN ($1, $2, $3, $4)"#,
        project_one_orphan.id,
        project_one_linked_target.id,
        project_two_orphan.id,
        project_two_linked_target.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query!(
        "UPDATE notes SET content_hash = NULL WHERE id IN ($1, $2, $3, $4)",
        project_one_canonical_hash.id,
        project_one_legacy_hash.id,
        project_two_canonical_hash.id,
        project_two_legacy_hash.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    HousekeepingFixture {
        projects: vec![
            HousekeepingFixtureProject {
                project: project_one,
                path: project_one_path,
                expected: HousekeepingFixtureExpectedCounts {
                    prune_associations: 1,
                    flag_orphan_notes: 1,
                    rebuild_missing_content_hashes: 2,
                    repair_broken_wikilinks: 1,
                    archive_audit_candidates: 0,
                },
                orphan_note_id: project_one_orphan.id,
                repaired_source_note_id: project_one_repair_source.id,
                repaired_target_note_id: project_one_repair_target.id,
                legacy_hash_note_ids: vec![
                    project_one_canonical_hash.id,
                    project_one_legacy_hash.id,
                ],
            },
            HousekeepingFixtureProject {
                project: project_two,
                path: project_two_path,
                expected: HousekeepingFixtureExpectedCounts {
                    prune_associations: 1,
                    flag_orphan_notes: 1,
                    rebuild_missing_content_hashes: 2,
                    repair_broken_wikilinks: 1,
                    archive_audit_candidates: 0,
                },
                orphan_note_id: project_two_orphan.id,
                repaired_source_note_id: project_two_repair_source.id,
                repaired_target_note_id: project_two_repair_target.id,
                legacy_hash_note_ids: vec![
                    project_two_canonical_hash.id,
                    project_two_legacy_hash.id,
                ],
            },
        ],
    }
}

/// Ensure the `doctor_findings` table exists in the database. The test DB is
/// cloned from `djinn_test_template` which may not include the latest migration
/// if the template hasn't been rebuilt. Existing template tables are upgraded
/// with the current additive doctor-finding columns and indexes.
///
/// Placed inside `djinn-db::test_support` so all raw SQL stays within the
/// `djinn-db` crate boundary (enforced by the raw-SQL boundary CI check).
pub async fn ensure_doctor_findings_schema(db: &Database) {
    db.ensure_initialized().await.expect("db initialized");
    // Check if the table exists; create it if not.
    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'doctor_findings'",
    )
    .fetch_optional(db.pool())
    .await
    .expect("check doctor_findings existence");

    if !matches!(exists, Some((count,)) if count > 0) {
        // Apply the base migration SQL inline. The ALTER/index below bring an
        // older cloned template forward to the current doctor-finding schema.
        sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS doctor_findings (
            id                VARCHAR(36)  NOT NULL PRIMARY KEY,
            run_id            VARCHAR(64)  NULL,
            created_at        VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
            check_name        VARCHAR(255) NOT NULL,
            severity          VARCHAR(16)  NOT NULL,
            entity_ids        JSONB        NOT NULL DEFAULT '[]'::jsonb,
            evidence          JSONB        NOT NULL DEFAULT '{}'::jsonb,
            resolver_snapshot JSONB        NULL,
            detail            TEXT         NULL,
            deduplication_key VARCHAR(255) NULL,
            CONSTRAINT doctor_findings_severity_check
                CHECK (severity IN ('info', 'warn', 'critical'))
        )"#,
        )
        .execute(db.pool())
        .await
        .expect("create doctor_findings table");
    }

    sqlx::query(
        "ALTER TABLE doctor_findings ADD COLUMN IF NOT EXISTS deduplication_key VARCHAR(255) NULL",
    )
    .execute(db.pool())
    .await
    .expect("add doctor_findings.deduplication_key");

    sqlx::query("CREATE INDEX IF NOT EXISTS doctor_findings_created_at_idx ON doctor_findings (created_at DESC)")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_created_at_idx");
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS doctor_findings_check_name_idx ON doctor_findings (check_name)",
    )
    .execute(db.pool())
    .await
    .expect("create doctor_findings_check_name_idx");
    sqlx::query("CREATE INDEX IF NOT EXISTS doctor_findings_check_name_created_at_idx ON doctor_findings (check_name, created_at DESC)")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_check_name_created_at_idx");
    sqlx::query("CREATE INDEX IF NOT EXISTS doctor_findings_entity_ids_gin_idx ON doctor_findings USING GIN (entity_ids jsonb_path_ops)")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_entity_ids_gin_idx");
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS doctor_findings_deduplication_key_unique ON doctor_findings (deduplication_key) WHERE deduplication_key IS NOT NULL")
        .execute(db.pool())
        .await
        .expect("create doctor_findings_deduplication_key_unique");
}

/// Persist a running session whose `task_run_id` has no matching ledger row.
///
/// This narrowly scoped fixture reproduces durable identities written before
/// the task-run foreign-key invariant existed. Trigger manipulation stays
/// behind the `djinn-db` test-support boundary and is restored before this
/// helper reports a session-creation failure.
pub async fn seed_legacy_session_without_task_run_ledger_for_test(
    db: &Database,
    events: EventBus,
    params: CreateSessionParams<'_>,
) -> SessionRecord {
    sqlx::query("ALTER TABLE sessions DISABLE TRIGGER ALL")
        .execute(db.pool())
        .await
        .expect("disable session FK triggers for legacy fixture");

    let created = SessionRepository::new(db.clone(), events)
        .create(params)
        .await;

    sqlx::query("ALTER TABLE sessions ENABLE TRIGGER ALL")
        .execute(db.pool())
        .await
        .expect("restore session FK triggers after legacy fixture");

    created.expect("create legacy session without task-run ledger")
}

/// Overwrite the `encrypted_value` column of a credential row with arbitrary
/// raw bytes. Used by tests that need to simulate decryption failures (corrupt
/// or truncated ciphertext) without going through the encrypt/decrypt round-trip.
///
/// This is a **test-only** escape hatch — all production writes MUST go through
/// the `CredentialRepository` boundary in `crate::repositories::credential`.
pub async fn corrupt_credential_encrypted_value(db: &Database, key_name: &str, raw_bytes: Vec<u8>) {
    sqlx::query("UPDATE credentials SET encrypted_value = $1 WHERE key_name = $2")
        .bind(raw_bytes)
        .bind(key_name)
        .execute(db.pool())
        .await
        .expect("corrupt_credential_encrypted_value: update failed");
}

// ── Seed helpers for memory-eval fixture loading ────────────────────────
// These insert rows with explicit timestamps, status, and confidence for
// the deterministic memory-eval benchmark.  The eval loader cannot use the
// standard repository `create` methods because those auto-generate
// timestamps and trigger wikilink indexing / event emission.

/// Insert an eval note with explicit timestamps, status, and confidence,
/// then fetch and return the resulting [`Note`] row.
///
/// **Not for production use.**  Used only by the memory-eval fixture loader.
#[allow(clippy::too_many_arguments)]
pub async fn seed_eval_note(
    db: &Database,
    id: &str,
    project_id: &str,
    permalink: &str,
    title: &str,
    note_type: &str,
    folder: &str,
    tags_json: &serde_json::Value,
    content: &str,
    retrieval_anchor: Option<&str>,
    content_hash: &str,
    created_at: &str,
    updated_at: &str,
    last_accessed: &str,
    status: &str,
    confidence: f64,
) -> Note {
    db.ensure_initialized().await.unwrap();
    let empty_scope: serde_json::Value = serde_json::json!([]);

    sqlx::query(
        r#"INSERT INTO notes
            (id, project_id, permalink, title, file_path,
             storage, note_type, folder, tags, content, retrieval_anchor,
             content_hash, scope_paths,
             created_at, updated_at, last_accessed,
             status, confidence, abstract, overview, access_count)
         VALUES ($1, $2, $3, $4, '',
                 'db', $5, $6, $7, $8, $9,
                 $10, $11,
                 $12, $13, $14,
                 $15, $16, NULL, NULL, 0)"#,
    )
    .bind(id)
    .bind(project_id)
    .bind(permalink)
    .bind(title)
    .bind(note_type)
    .bind(folder)
    .bind(tags_json)
    .bind(content)
    .bind(retrieval_anchor)
    .bind(content_hash)
    .bind(&empty_scope)
    .bind(created_at)
    .bind(updated_at)
    .bind(last_accessed)
    .bind(status)
    .bind(confidence)
    .execute(db.pool())
    .await
    .unwrap_or_else(|e| panic!("seed_eval_note: failed to insert note '{permalink}': {e}"));

    // Fetch the note back using the same SELECT projection the rest of
    // djinn-db uses (mirrors the `note_select_where_id!` macro).
    sqlx::query_as::<_, Note>(
        r#"SELECT id, project_id, permalink, title, file_path,
                  storage, note_type, folder, status, tags::text AS tags, content,
                  retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                  access_count, confidence, abstract as abstract_, overview,
                  scope_paths::text AS scope_paths
           FROM notes WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(db.pool())
    .await
    .unwrap_or_else(|e| panic!("seed_eval_note: failed to fetch note '{permalink}': {e}"))
}

/// Insert an eval epic row and return its id.
///
/// **Not for production use.**  Used only by the memory-eval fixture loader.
pub async fn seed_eval_epic(db: &Database, project_id: &str, title: &str) -> String {
    db.ensure_initialized().await.unwrap();
    let epic_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("ep-{}", &epic_id[epic_id.len() - 12..]);
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)\n         VALUES ($1, $2, $3, $4, '', '', '', '', '[]'::jsonb)",
    )
    .bind(&epic_id)
    .bind(project_id)
    .bind(&short_id)
    .bind(title)
    .execute(db.pool())
    .await
    .expect("seed_eval_epic: failed to create epic");
    epic_id
}

/// Insert an eval task with `memory_refs` pointing to note IDs (for
/// task-affinity scoring) and return the generated task id.
///
/// **Not for production use.**  Used only by the memory-eval fixture loader.
pub async fn seed_eval_task_with_memory_refs(
    db: &Database,
    project_id: &str,
    epic_id: &str,
    fixture_task_id: &str,
    memory_refs_json: &str,
) -> String {
    db.ensure_initialized().await.unwrap();
    let creator = seed_test_user(db).await;
    let task_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!(
        "eval-{}",
        fixture_task_id.chars().take(8).collect::<String>()
    );

    sqlx::query(
        r#"INSERT INTO tasks
            (id, project_id, short_id, epic_id, title, description, design,
             issue_type, priority, owner, status, continuation_count, memory_refs,
             created_by_user_id)
         VALUES ($1, $2, $3, $4, $5, '', '', 'task', 0, '', 'open', 0, $6::jsonb, $7)"#,
    )
    .bind(&task_id)
    .bind(project_id)
    .bind(&short_id)
    .bind(epic_id)
    .bind(format!("Eval task {}", fixture_task_id))
    .bind(memory_refs_json)
    .bind(&creator)
    .execute(db.pool())
    .await
    .unwrap_or_else(|e| {
        panic!("seed_eval_task_with_memory_refs: failed to create task '{fixture_task_id}': {e}")
    });

    task_id
}
