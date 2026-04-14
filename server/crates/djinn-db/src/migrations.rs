use std::path::Path;

mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations");
}

const SQLITE_SCHEMA_SNAPSHOT: &str = include_str!("../schema.sql");
const MYSQL_SCHEMA_SNAPSHOT: &str = include_str!("../sql/mysql_schema.sql");
const MYSQL_NOTES_FULLTEXT_PROTOTYPE: &str =
    include_str!("../sql/mysql_notes_fulltext_prototype.sql");

/// Run migrations using refinery's built-in rusqlite runner.
///
/// Refinery handles checksum validation (rejects modified migrations) and
/// ordering enforcement (rejects out-of-order versions) automatically.
pub fn run(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = rusqlite::Connection::open(path)?;
    embedded::migrations::runner().run(&mut conn)?;
    Ok(())
}

/// Return the canonical SQLite schema snapshot used by the current runtime.
pub fn sqlite_schema_snapshot() -> &'static str {
    SQLITE_SCHEMA_SNAPSHOT
}

/// Return the staged Dolt/MySQL schema snapshot for ADR-055 note/task/session state.
///
/// This artifact is intentionally separate from the embedded SQLite refinery migrations so the
/// existing SQLite runtime remains selectable while the MySQL/Dolt migration path is explicit.
pub fn mysql_schema_snapshot() -> &'static str {
    MYSQL_SCHEMA_SNAPSHOT
}

/// Return the reference MySQL FULLTEXT query prototype paired with the staged schema snapshot.
pub fn mysql_notes_fulltext_prototype() -> &'static str {
    MYSQL_NOTES_FULLTEXT_PROTOTYPE
}

/// Return the embedded migration list (version, name, checksum) for testing.
#[cfg(test)]
pub(crate) fn embedded_checksums() -> Vec<(i64, String, u64)> {
    embedded::migrations::runner()
        .get_migrations()
        .iter()
        .map(|m| (m.version(), m.name().to_string(), m.checksum()))
        .collect()
}

#[cfg(test)]
pub(crate) fn run_until(
    path: &Path,
    migration_name_exclusive: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = rusqlite::Connection::open(path)?;
    let runner = embedded::migrations::runner();
    let migrations = runner.get_migrations();
    let stop_version = migrations
        .iter()
        .find(|migration| migration.name() == migration_name_exclusive)
        .map(|migration| migration.version() - 1)
        .unwrap_or_else(|| {
            migrations
                .last()
                .map(|migration| migration.version())
                .unwrap_or(0)
        });

    runner
        .set_target(refinery::Target::Version(stop_version))
        .run(&mut conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{mysql_notes_fulltext_prototype, mysql_schema_snapshot, sqlite_schema_snapshot};

    #[test]
    fn sqlite_snapshot_retains_sqlite_specific_search_structures() {
        let schema = sqlite_schema_snapshot();
        assert!(schema.contains("CREATE VIRTUAL TABLE notes_fts USING fts5"));
        assert!(schema.contains("CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN"));
    }

    #[test]
    fn mysql_snapshot_replaces_fts_shadow_table_with_fulltext_index() {
        let schema = mysql_schema_snapshot();
        assert!(schema.contains("ALTER TABLE notes ADD FULLTEXT KEY notes_ft"));
        assert!(!schema.contains("CREATE VIRTUAL TABLE notes_fts USING fts5"));
        assert!(!schema.contains("CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN"));
        assert!(!schema.contains("vec0("));
    }

    #[test]
    fn mysql_artifacts_document_clear_parallel_cutover_path() {
        let schema = mysql_schema_snapshot();
        let prototype = mysql_notes_fulltext_prototype();

        assert!(schema.contains("CREATE TABLE tasks"));
        assert!(schema.contains("CREATE TABLE notes"));
        assert!(schema.contains("CREATE TABLE sessions"));
        assert!(prototype.contains("MATCH(n.title, n.content, n.tags) AGAINST"));
    }
}
