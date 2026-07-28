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
