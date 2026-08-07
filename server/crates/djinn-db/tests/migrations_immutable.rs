//! Enforce that committed Postgres migrations are canonically named with
//! strictly-increasing version prefixes. sqlx records a per-file checksum
//! in `_sqlx_migrations` on first apply and will refuse to restart if a
//! previously-applied file is later mutated — that runtime guarantee is
//! verified in integration tests that exercise the live Postgres server;
//! this unit test just pins the file-naming contract.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn migrations_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn canonical_entries(dir: &Path) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    for entry in fs::read_dir(dir).expect("migrations dir readable") {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".sql") {
            continue;
        }
        let stem = name.trim_end_matches(".sql");
        let (version_str, _) = stem
            .split_once('_')
            .unwrap_or_else(|| panic!("migration `{name}` does not follow `{{N}}_{{slug}}.sql`"));
        let version: u64 = version_str
            .parse()
            .unwrap_or_else(|_| panic!("migration `{name}` has non-integer version prefix"));
        out.push((version, name));
    }
    out.sort();
    out
}

#[test]
fn postgres_migration_names_are_canonical_and_increasing() {
    let entries = canonical_entries(&migrations_dir("migrations_postgres"));
    assert!(
        !entries.is_empty(),
        "must have at least one postgres migration"
    );
    let mut last = 0_u64;
    for (v, _) in &entries {
        assert!(
            *v > last,
            "postgres migration versions must strictly increase; saw {v} after {last}"
        );
        last = *v;
    }
}

#[test]
fn postgres_migration_150_is_immutable() {
    let bytes = fs::read(
        migrations_dir("migrations_postgres").join("150_service_preset_wrapper_identity.sql"),
    )
    .expect("migration 150 readable");
    let digest = Sha256::digest(bytes);

    assert_eq!(
        format!("{digest:x}"),
        "c341372f7b745f384175ef412d1d40a7c809b8194930cf16533eb638d49f3dc1"
    );
}

#[test]
fn postgres_migration_167_is_immutable() {
    let bytes =
        fs::read(migrations_dir("migrations_postgres").join("167_launcher_authority_mode.sql"))
            .expect("migration 167 readable");
    let digest = Sha256::digest(bytes);

    assert_eq!(
        format!("{digest:x}"),
        "137a03e948625947052b6aeed64dea53efa6a99184c30fd832815619e26edae7"
    );
}

/// 193 is pinned because `scripts/check-migrations-immutable.sh` structurally
/// cannot catch an edit to it from a branch in the `nafu` stack.
///
/// That guard diffs `origin/main...HEAD`, i.e. against the MERGE BASE. Wave 1
/// added 193 on its own branch and was squash-merged to main afterwards, so
/// every branch stacked on wave 1 forked from a commit where 193 did not yet
/// exist. The three-dot diff therefore classifies 193 as *Added* by the branch
/// (`--diff-filter=MRD` deliberately excludes `A`) and the guard reports OK
/// while the file is, in fact, being edited after main already had it. That is
/// exactly what happened: wave 5 grew 193 by +107/-1 and the guard stayed green.
///
/// A content hash does not care about the merge base, so it holds where the
/// diff-based guard cannot. Corrective DDL goes in a NEW migration --
/// `195_ci_route_lead_rejection_and_rollback_reports.sql` is where the wave-5
/// delta actually lives.
#[test]
fn postgres_migration_193_is_immutable() {
    let bytes = fs::read(migrations_dir("migrations_postgres").join("193_ci_route_attempts.sql"))
        .expect("migration 193 readable");
    let digest = Sha256::digest(bytes);

    assert_eq!(
        format!("{digest:x}"),
        "f8d980da51aabd745bc89f163109ba23ee1198a76d5ae8d2271290f4321d906c",
        "migration 193 was modified after it merged to main. sqlx stores its \
         checksum in `_sqlx_migrations` and every database that already applied \
         it will refuse to boot. Restore it byte-for-byte \
         (`git checkout origin/main -- \
         server/crates/djinn-db/migrations_postgres/193_ci_route_attempts.sql`) \
         and express the change as a new migration instead."
    );
}
