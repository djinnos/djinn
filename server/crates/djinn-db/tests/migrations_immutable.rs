//! Enforce that committed migrations are immutable.
//!
//! sqlx records a checksum of each migration's bytes in `_sqlx_migrations`
//! on first apply. If the file is later mutated and re-applied against the
//! same DB, sqlx returns `MigrateError::VersionMismatch`. That's the exact
//! guarantee we want: "never edit an applied migration — always add a new
//! one". This test proves the mechanism is active end-to-end.
//!
//! It also sanity-checks that every file in the shipped migration dirs
//! has a canonical `{N}_{slug}.sql` name with strictly-increasing versions.

use std::fs;
use std::path::{Path, PathBuf};

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::sqlite::SqlitePoolOptions;

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
fn sqlite_migration_names_are_canonical_and_increasing() {
    let entries = canonical_entries(&migrations_dir("migrations_sqlite"));
    assert!(!entries.is_empty(), "must have at least one sqlite migration");
    let mut last = 0_u64;
    for (v, _) in &entries {
        assert!(
            *v > last,
            "sqlite migration versions must strictly increase; saw {v} after {last}"
        );
        last = *v;
    }
}

#[test]
fn mysql_migration_names_are_canonical_and_increasing() {
    let entries = canonical_entries(&migrations_dir("migrations_mysql"));
    assert!(!entries.is_empty(), "must have at least one mysql migration");
    let mut last = 0_u64;
    for (v, _) in &entries {
        assert!(
            *v > last,
            "mysql migration versions must strictly increase; saw {v} after {last}"
        );
        last = *v;
    }
}

/// Apply a copy of the sqlite migrations, then mutate the copy and re-apply
/// against the same DB. sqlx must refuse with `VersionMismatch`.
#[tokio::test]
async fn mutating_an_applied_migration_is_rejected() {
    let src = migrations_dir("migrations_sqlite");

    // Stage a mutable copy of the migrations directory so we can edit a file
    // after the first apply without dirtying the committed tree.
    let tmp = tempfile::tempdir().expect("tempdir");
    let staged = tmp.path().join("migrations_sqlite");
    fs::create_dir_all(&staged).unwrap();
    for entry in fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        fs::copy(entry.path(), staged.join(&name)).unwrap();
    }

    let db_file = tmp.path().join("immutability.db");
    let dsn = format!("sqlite://{}?mode=rwc", db_file.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&dsn)
        .await
        .expect("open sqlite pool");

    // Initial apply must succeed.
    let migrator = Migrator::new(staged.as_path())
        .await
        .expect("load migrator");
    migrator.run(&pool).await.expect("first apply");

    // Mutate the first migration (append a trailing comment — harmless to SQL
    // but changes the checksum sqlx stored).
    let first = canonical_entries(&staged)
        .into_iter()
        .next()
        .expect("at least one migration");
    let path = staged.join(&first.1);
    let mut contents = fs::read_to_string(&path).unwrap();
    contents.push_str("\n-- mutated-by-test\n");
    fs::write(&path, contents).unwrap();

    let mutated = Migrator::new(staged.as_path())
        .await
        .expect("reload migrator");
    let err = mutated
        .run(&pool)
        .await
        .expect_err("re-apply of mutated migration must fail");
    assert!(
        matches!(err, MigrateError::VersionMismatch(_)),
        "expected VersionMismatch, got {err:?}"
    );
}
