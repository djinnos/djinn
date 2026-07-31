//! The enum and the database must agree, and the agreement must be checkable.
//!
//! Migration 164 constrains `observed_launcher_protocol` and
//! `effective_launcher_protocol` to a literal `IN (...)` list. Before task 78v1
//! that list was restated three times — in the migration, in
//! `build_pod_permit.rs`'s two `matches!` arms, and (as a *different* type with
//! no wire form at all) inside `djinn-cgroup-launcher`. Nothing forced the three
//! to agree, and #2800 shipped precisely because nothing did.
//!
//! These tests iterate `LauncherAuthorityProtocol::ALL`, so adding a variant
//! without teaching migration 164 about it fails here rather than in production
//! on the first `CHECK` violation.

use std::path::{Path, PathBuf};

use djinn_db::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, BuildPodPermitRepository,
    BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult, Database,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn migration_164() -> String {
    let path = crate_root().join("migrations_postgres/164_build_pod_resize_identity.sql");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("migration 164 must be readable at {path:?}: {error}"))
}

/// Pull the literal set out of `<column> IN ('a', 'b')`.
fn check_constraint_set(sql: &str, column: &str) -> Vec<String> {
    let needle = format!("{column} IN (");
    let start = sql
        .find(&needle)
        .unwrap_or_else(|| panic!("migration 164 must constrain {column} with an IN list"))
        + needle.len();
    let end = start
        + sql[start..]
            .find(')')
            .expect("the IN list must be closed in the same statement");
    sql[start..end]
        .split(',')
        .map(|literal| literal.trim().trim_matches('\'').to_owned())
        .collect()
}

/// **AC5, static half.** Every variant's wire string is in migration 164's
/// `CHECK`, and the `CHECK` names nothing the enum cannot produce.
///
/// Equality in *both* directions is the point. A subset assertion would let a
/// third variant be added silently; a superset assertion would let a wire
/// string be removed from the enum while rows carrying it stayed legal.
#[test]
fn migration_164_accepts_exactly_the_protocols_the_enum_can_produce() {
    let sql = migration_164();
    let expected: Vec<String> = LauncherAuthorityProtocol::ALL
        .iter()
        .map(|protocol| protocol.as_wire().to_owned())
        .collect();

    for column in ["observed_launcher_protocol", "effective_launcher_protocol"] {
        assert_eq!(
            check_constraint_set(&sql, column),
            expected,
            "{column}'s CHECK list and the protocol enum have diverged"
        );
    }
}

/// **AC5, no-duplication half.** The repository validates through `FromStr`,
/// not through its own copy of the literals.
///
/// The needles are assembled at compile time so this file does not match
/// itself.
#[test]
fn the_permit_repository_routes_protocol_validation_through_from_str() {
    let path = crate_root().join("src/repositories/build_pod_permit.rs");
    let source = std::fs::read_to_string(&path).expect("the repository source must be readable");

    for literal in [
        concat!('"', "leaf-", "v1", '"'),
        concat!('"', "resize-", "v2", '"'),
    ] {
        assert!(
            !source.contains(literal),
            "build_pod_permit.rs must not restate {literal}; migration 164 and \
             LauncherAuthorityProtocol are the two authorities and neither is this file"
        );
    }
    assert!(
        source.contains(concat!(
            "parse::<djinn_launcher_protocol::",
            "LauncherAuthorityProtocol>"
        )),
        "build_pod_permit.rs must validate the protocol columns through the canonical FromStr"
    );
}

/// **AC5, durable half.** Every variant is actually accepted by the live
/// migration-164 `CHECK`, through the same repository call production uses.
///
/// The static test above reads the migration as text; this one makes Postgres
/// answer. A string that parses but that the deployed schema rejects would pass
/// the first test and fail here.
#[tokio::test]
async fn every_protocol_variant_is_accepted_by_the_live_check_constraint() {
    let db = Database::ephemeral().await.unwrap();
    let run_ids: Vec<String> = LauncherAuthorityProtocol::ALL
        .iter()
        .map(|protocol| format!("protocol-agreement-{}", protocol.as_wire()))
        .collect();
    seed_runs(&db, &run_ids).await;
    let repository = BuildPodPermitRepository::new(db.clone());

    for (index, protocol) in LauncherAuthorityProtocol::ALL.iter().enumerate() {
        let wire = protocol.as_wire();
        let run = run_ids[index].as_str();
        // One live permit per variant: the pool cap is a concurrency control,
        // not the constraint under test.
        let permits = LauncherAuthorityProtocol::ALL.len() as i64;
        let permit = match repository.acquire(run, permits).await {
            AcquireBuildPodPermitResult::Acquired { row, .. } => row,
            outcome => panic!("{wire}: unexpected acquire outcome: {outcome:?}"),
        };
        assert!(matches!(
            repository
                .bind_or_refresh_job_uid(run, &permit.permit_id, permit.fencing_token, run)
                .await
                .unwrap(),
            BindBuildPodPermitResult::Bound(_)
        ));

        let identity = identity_for(run, wire, wire);
        let captured = repository
            .capture_resize_identity(run, &permit.permit_id, permit.fencing_token, &identity)
            .await
            .unwrap();
        let CaptureBuildPodResizeIdentityResult::Captured(row) = captured else {
            panic!("{wire} must be accepted by migration 164's CHECK, got {captured:?}");
        };
        let stored = row
            .resize_identity
            .unwrap_or_else(|| panic!("{wire}: a captured permit must carry its identity"));
        assert_eq!(stored.observed_launcher_protocol, wire);
        assert_eq!(stored.effective_launcher_protocol, wire);
        assert_eq!(
            stored.observed_launcher_protocol.parse(),
            Ok(*protocol),
            "{wire} must round-trip back into the variant it came from"
        );
    }
}

/// A protocol the enum cannot produce is refused before it reaches the
/// database, so the `CHECK` list and `FromStr` cannot silently diverge in the
/// permissive direction either.
#[tokio::test]
async fn a_protocol_the_enum_cannot_produce_is_refused() {
    let db = Database::ephemeral().await.unwrap();
    seed_runs(&db, &["protocol-agreement-rejected".to_owned()]).await;
    let repository = BuildPodPermitRepository::new(db.clone());
    let run = "protocol-agreement-rejected";
    let permit = match repository.acquire(run, 1).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        outcome => panic!("unexpected acquire outcome: {outcome:?}"),
    };
    assert!(matches!(
        repository
            .bind_or_refresh_job_uid(run, &permit.permit_id, permit.fencing_token, run)
            .await
            .unwrap(),
        BindBuildPodPermitResult::Bound(_)
    ));

    for (observed, effective) in [
        ("resize-v3", "resize-v2"),
        ("resize-v2", "unknown-v9"),
        ("Resize-V2", "resize-v2"),
        ("resize-v2 ", "resize-v2"),
        ("", "resize-v2"),
    ] {
        let identity = identity_for(run, observed, effective);
        let outcome = repository
            .capture_resize_identity(run, &permit.permit_id, permit.fencing_token, &identity)
            .await
            .unwrap();
        assert!(
            matches!(outcome, CaptureBuildPodResizeIdentityResult::Rejected),
            "({observed:?}, {effective:?}) must be refused, got {outcome:?}"
        );
    }
}

fn identity_for(run: &str, observed: &str, effective: &str) -> BuildPodResizeIdentity {
    BuildPodResizeIdentity {
        pod_namespace: "ns".into(),
        pod_name: format!("pod-{run}"),
        pod_uid: format!("uid-{run}"),
        launcher_container_name: "launcher".into(),
        launcher_container_id: format!("containerd://{run}"),
        image_digest: "sha256:agreement".into(),
        observed_launcher_protocol: observed.into(),
        effective_launcher_protocol: effective.into(),
        admitted_cpu_millicores: 1000,
    }
}

async fn seed_runs(db: &Database, ids: &[String]) {
    db.ensure_initialized().await.unwrap();
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) \
         VALUES ('00000000-0000-7000-8000-000000000078', 9000000078, 'protocol-agreement')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('protocol-agreement-project', 'protocol-agreement-project', 'djinnos', 'protocol-agreement')",
    )
    .execute(pool)
    .await
    .unwrap();
    for (index, id) in ids.iter().enumerate() {
        let task_id = format!("protocol-agreement-task-{index}");
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ($1, 'protocol-agreement-project', $2, 'title', 'description', 'design', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '00000000-0000-7000-8000-000000000078')",
        )
        .bind(&task_id)
        .bind(format!("proto-{index}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
             VALUES ($1, 'protocol-agreement-project', $2, 'manual', 'running')",
        )
        .bind(id)
        .bind(&task_id)
        .execute(pool)
        .await
        .unwrap();
    }
}
